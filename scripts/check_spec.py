"""Checks api/openapi.yaml against itself and its JSON-LD context files, with
no implementation involved: the examples fit their schemas, the schemas
refuse what they should, every field maps to the same term in the latest
context, and the context revisions are bookkept consistently (files, enum,
hashes, and every place the spec names a revision).

    pip install -r conformance/requirements.txt
    python -m pytest scripts/check_spec.py        # API_SPEC=<path> for another copy

It needs only the spec and the context files next to it, so it travels
with them to any implementation's repository.
"""

import hashlib
import http
import os
import pathlib
import re

import jsonschema
import pytest
import yaml

SPEC_PATH = pathlib.Path(os.environ.get("API_SPEC") or pathlib.Path(__file__).resolve().parent.parent / "api/openapi.yaml")
SPEC_TEXT = SPEC_PATH.read_text()
SPEC = yaml.safe_load(SPEC_TEXT)
API_DIR = SPEC_PATH.parent
COMPONENTS = SPEC["components"]
SCHEMAS = COMPONENTS["schemas"]
FORMAT_CHECKER = jsonschema.Draft202012Validator.FORMAT_CHECKER

LIST = "/api/v1/institutions"
CONTEXT = next(p for p in SPEC["paths"] if "{revision}" in p)
CONTEXT_GET = SPEC["paths"][CONTEXT]["get"]
REVISIONS = COMPONENTS["parameters"]["Revision"]["schema"]["enum"]
LATEST = max(int(r) for r in REVISIONS)
LINK_REL = 'rel="http://www.w3.org/ns/json-ld#context"; type="application/ld+json"'


def validator(name):
    schema = {"$schema": "https://json-schema.org/draft/2020-12/schema", "$ref": f"#/components/schemas/{name}", "components": COMPONENTS}
    return jsonschema.Draft202012Validator(schema, format_checker=FORMAT_CHECKER)


def errors(name, value):
    return [f"{e.message} at {list(e.absolute_path)}" for e in validator(name).iter_errors(value)]


def resolve(obj):
    while "$ref" in obj:
        *_, kind, name = obj["$ref"].split("/")
        obj = COMPONENTS[kind][name]
    return obj


def context_file(revision):
    return API_DIR / f"institutions.context.r{revision}.jsonld"


def latest_context():
    return yaml.safe_load(context_file(LATEST).read_text())["@context"]


def all_responses():
    """(where, response) for every response the spec defines, refs resolved."""
    for path, item in SPEC["paths"].items():
        for method, op in item.items():
            for status, resp in op["responses"].items():
                yield f"{method.upper()} {path} {status}", resolve(resp)
    for name, resp in COMPONENTS["responses"].items():
        yield f"components/responses/{name}", resp


# Examples


def test_list_examples_conform():
    examples = SPEC["paths"][LIST]["get"]["responses"]["200"]["content"]["application/json"]["examples"]
    assert examples
    for name, example in examples.items():
        assert not errors("InstitutionsResponse", example["value"]), name
        assert "mock" not in name, f"example {name!r} names the demo"


def test_problem_examples_conform():
    checked = 0
    for name, resp in COMPONENTS["responses"].items():
        for example in resp.get("content", {}).get("application/problem+json", {}).get("examples", {}).values():
            value = example["value"]
            assert not errors("Problem", value), name
            assert value["title"] == http.HTTPStatus(value["status"]).phrase, name
            checked += 1
    assert checked >= 7, f"problem examples checked: {checked}"


def test_every_problem_response_has_an_example():
    for name, resp in COMPONENTS["responses"].items():
        problem = resp.get("content", {}).get("application/problem+json")
        if problem:
            assert problem.get("examples"), name


# Schemas

GOOD = {
    "type": "Organization",
    "id": "a",
    "name": "A",
    "provider": "globus",
    "redirect_idp_domain": "a.edu",
    "idp_logout_url": "https://idp.a.edu/logout",
    "approved": True,
}


def test_a_good_entry_passes():
    assert not errors("Institution", GOOD)


@pytest.mark.parametrize(
    "field, bad",
    [
        ("id", "A"),
        ("id", "a" * 65),
        ("name", ""),
        ("name", "   "),
        ("redirect_idp_domain", ".."),
        ("redirect_idp_domain", "UChicago.edu"),
        ("redirect_idp_domain", "10.0.0.1"),
        ("idp_logout_url", "http://idp.a.edu/logout"),
        ("idp_logout_url", "https://"),
        ("idp_logout_url", "https://idp.a.edu/log out"),
        ("idp_logout_url", "https://a.edu/ü[x]"),
        ("idp_logout_url", "https://a.edu/a#b#c"),
        ("approved", False),
        ("type", "Thing"),
        ("provider", "Not-A-Value"),
    ],
)
def test_bad_entries_are_refused(field, bad):
    assert errors("Institution", {**GOOD, field: bad}), f"{field} = {bad!r}"


def test_tacc_tapis_entries_have_no_domain():
    assert errors("Institution", {**GOOD, "provider": "tacc_tapis"})
    assert not errors("Institution", {**GOOD, "provider": "tacc_tapis", "redirect_idp_domain": None})


def test_unknown_providers_are_accepted():
    # Provider is an open set: a later version may add values.
    assert not errors("Institution", {**GOOD, "provider": "some_future_flow"})


def test_data_version_is_etag_safe():
    def response(version):
        return {"type": "ItemList", "status": "success", "message": "", "data_version": version, "result": []}

    assert not errors("InstitutionsResponse", response("2026-10-23.1"))
    for bad in ["a,b", 'a"b', "a b", "", "a" * 65]:
        assert errors("InstitutionsResponse", response(bad)), repr(bad)


def test_etag_is_weak_and_names_the_context():
    etag = jsonschema.Draft202012Validator(COMPONENTS["headers"]["ETag"]["schema"])
    assert etag.is_valid(f'W/"2026-10-23.1;r{LATEST}"')
    for bad in ['"2026-10-23.1;r1"', 'W/"2026-10-23.1"', 'W/"2026;r0"', 'W/"a,b;r1"', 'W/";r1"']:
        assert not etag.is_valid(bad), bad


# Spec and context agree


def test_every_mapped_field_has_the_same_term_in_the_context():
    ctx = latest_context()

    def term(key):
        value = ctx.get(key)
        return value.get("@id") if isinstance(value, dict) else value

    for schema in ("InstitutionsResponse", "Institution"):
        for field, prop in SCHEMAS[schema]["properties"].items():
            mapped = prop.get("x-schema-org") or resolve(prop).get("x-schema-org")
            if mapped:
                assert term(field) == mapped, f"{schema}.{field}"
            else:
                assert field in ctx and ctx[field] is None, f"{schema}.{field} must be null in the context"
        type_name = SCHEMAS[schema]["properties"]["type"]["const"]
        assert term(type_name) == SCHEMAS[schema]["x-schema-org"]["@type"], f"{schema} type"


def test_every_known_provider_is_mapped_in_the_context():
    ctx = latest_context()
    known = SCHEMAS["Provider"]["x-known-values"]
    assert known and known == SCHEMAS["Provider"]["examples"]
    pattern = re.compile(SCHEMAS["Provider"]["pattern"])
    for value in known:
        assert pattern.search(value), value
        assert isinstance(ctx.get(value), str) and ctx[value].startswith("tms:"), value


# Context revisions


def test_revisions_run_without_gaps():
    assert REVISIONS == [str(n) for n in range(1, len(REVISIONS) + 1)]


def test_context_files_are_exactly_the_revisions():
    files = sorted(p.name for p in API_DIR.glob("institutions.context.*"))
    assert files == sorted(context_file(r).name for r in REVISIONS)


def test_spec_publishes_each_revisions_hash():
    published = CONTEXT_GET["x-context-sha256"]
    assert sorted(published) == sorted(REVISIONS)
    for revision in REVISIONS:
        body = context_file(revision).read_text().replace("\r\n", "\n").encode()
        assert hashlib.sha256(body).hexdigest() == published[revision], f"r{revision} changed: publish it as r{LATEST + 1} instead"


def test_context_example_is_the_latest_revision():
    example = CONTEXT_GET["responses"]["200"]["content"]["application/ld+json"]["examples"]["latest"]
    assert example["summary"] == f"Revision {LATEST}"
    assert example["value"] == yaml.safe_load(context_file(LATEST).read_text())


def test_link_example_names_the_latest_revision():
    base = SPEC["servers"][0]["variables"]["base_url"]["default"]
    expected = f"<{base}{CONTEXT.format(revision=LATEST)}>; {LINK_REL}"
    assert COMPONENTS["headers"]["ContextLink"]["example"] == expected


def test_spec_names_only_the_latest_revision():
    context_prefix = CONTEXT.split("{")[0]
    mentions = [int(m) for m in re.findall(re.escape(context_prefix) + r"(\d+)", SPEC_TEXT)]
    mentions += [int(m) for m in re.findall(r';r(\d+)"', SPEC_TEXT)]
    assert mentions, "the spec names the latest revision"
    assert set(mentions) == {LATEST}, f"the spec names {sorted(set(mentions))}, not only the latest r{LATEST}"


# Headers


def test_allow_headers_name_the_paths_methods():
    for path, header in [(LIST, "ListAllow"), (CONTEXT, "ContextAllow")]:
        methods = {m.upper() for m in SPEC["paths"][path]}
        allow = {m.strip() for m in COMPONENTS["headers"][header]["schema"]["const"].split(",")}
        assert allow == methods, path


def test_every_response_forbids_sniffing():
    for where, resp in all_responses():
        headers = {name.lower(): resolve(h) for name, h in resp.get("headers", {}).items()}
        assert headers.get("x-content-type-options") is COMPONENTS["headers"]["NoSniff"], where


def test_problem_responses_are_never_stored():
    for where, resp in all_responses():
        if "application/problem+json" in resp.get("content", {}):
            assert resolve(resp["headers"]["Cache-Control"]) is COMPONENTS["headers"]["ErrorCacheControl"], where
