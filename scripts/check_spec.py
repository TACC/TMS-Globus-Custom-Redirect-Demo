"""Checks api/openapi.yaml and its addendum against themselves and the
JSON-LD context files, with no implementation involved: the examples fit
their schemas, the schemas refuse what they should, every field has a term
(or null) in the latest context, and the context revisions are bookkept
consistently (files, enum, hashes, and every place the addendum names a
revision).

    pip install -r conformance/requirements.txt
    python -m pytest scripts/check_spec.py        # API_SPEC=<path> for another copy

It needs only the spec, the addendum and the context files next to them,
so it travels with them to any implementation's repository.
"""

import hashlib
import os
import pathlib
import re

import jsonschema
import pytest
import yaml

SPEC_PATH = pathlib.Path(os.environ.get("API_SPEC") or pathlib.Path(__file__).resolve().parent.parent / "api/openapi.yaml")
API_DIR = SPEC_PATH.parent
SPEC = yaml.safe_load(SPEC_PATH.read_text())
ADDENDUM_PATH = API_DIR / "openapi.addendum.yaml"
ADDENDUM_TEXT = ADDENDUM_PATH.read_text()
ADDENDUM = yaml.safe_load(ADDENDUM_TEXT)
DOCS = {"openapi.yaml": SPEC, "openapi.addendum.yaml": ADDENDUM}
COMPONENTS = SPEC["components"]
SCHEMAS = COMPONENTS["schemas"]
FORMAT_CHECKER = jsonschema.Draft202012Validator.FORMAT_CHECKER

LIST = "/v1/tms/institutions"
CONTEXT = next(p for p in ADDENDUM["paths"] if "{revision}" in p)
CONTEXT_GET = ADDENDUM["paths"][CONTEXT]["get"]
REVISIONS = ADDENDUM["components"]["parameters"]["Revision"]["schema"]["enum"]
LATEST = max(int(r) for r in REVISIONS)
LINK_REL = 'rel="http://www.w3.org/ns/json-ld#context"; type="application/ld+json"'


def validator(name, doc=SPEC):
    schema = {"$schema": "https://json-schema.org/draft/2020-12/schema", "$ref": f"#/components/schemas/{name}", "components": doc["components"]}
    return jsonschema.Draft202012Validator(schema, format_checker=FORMAT_CHECKER)


def errors(name, value, doc=SPEC):
    return [f"{e.message} at {list(e.absolute_path)}" for e in validator(name, doc).iter_errors(value)]


def resolve(obj, doc):
    """`obj` with its $refs followed, within `doc` or into the core spec."""
    while "$ref" in obj:
        file, _, pointer = obj["$ref"].partition("#")
        if file:
            doc = DOCS[pathlib.PurePosixPath(file).name]
        *_, kind, name = pointer.split("/")
        obj = doc["components"][kind][name]
    return obj


def context_file(revision):
    return API_DIR / f"institutions.context.r{revision}.jsonld"


def latest_context():
    return yaml.safe_load(context_file(LATEST).read_text())["@context"]


def all_responses():
    """(where, response, doc) for every response either document defines."""
    for doc_name, doc in DOCS.items():
        for path, item in doc["paths"].items():
            for method, op in item.items():
                for status, resp in op["responses"].items():
                    yield f"{doc_name} {method.upper()} {path} {status}", resp, doc
        for name, resp in doc["components"].get("responses", {}).items():
            yield f"{doc_name} components/responses/{name}", resp, doc


def list_example(doc):
    return doc["paths"][LIST]["get"]["responses"]["200"]["content"]["application/json"]["examples"]


# Examples


def test_list_examples_conform():
    for doc in DOCS.values():
        examples = list_example(doc)
        assert examples
        for name, example in examples.items():
            assert not errors("InstitutionsResponse", example["value"]), name
            assert "mock" not in name, f"example {name!r} names the demo"


def test_addendum_example_is_the_core_example_with_types():
    for name, example in list_example(ADDENDUM).items():
        assert not errors("LinkedList", example["value"], ADDENDUM), name
        core = list_example(SPEC)[name]["value"]
        untyped = {k: v for k, v in example["value"].items() if k != "type"}
        untyped["institutions"] = [{k: v for k, v in e.items() if k != "type"} for e in untyped["institutions"]]
        assert untyped == core, name


def error_responses():
    """(where, status, response, doc) for every response whose body is an
    HttpResult; status is None for a component response."""
    for doc_name, doc in DOCS.items():
        for path, item in doc["paths"].items():
            for method, op in item.items():
                for status, resp in op["responses"].items():
                    if is_error(resolve(resp, doc)):
                        yield f"{doc_name} {method.upper()} {path} {status}", status, resolve(resp, doc), doc
        for name, resp in doc["components"].get("responses", {}).items():
            if is_error(resp):
                yield f"{doc_name} components/responses/{name}", None, resp, doc


def is_error(resp):
    schema = resp.get("content", {}).get("application/json", {}).get("schema", {})
    return schema.get("$ref", "").endswith("/schemas/HttpResult")


def test_error_examples_conform():
    checked = 0
    for where, status, resp, _ in error_responses():
        examples = resp["content"]["application/json"].get("examples")
        assert examples, f"{where} has an example"
        for example in examples.values():
            value = example["value"]
            assert not errors("HttpResult", value), where
            if status:
                assert value["result_code"] == status, f"{where}: result_code is the status"
            checked += 1
    assert checked >= 3, f"error examples checked: {checked}"


def test_a_body_is_an_http_result_exactly_when_it_is_an_error():
    # The addendum's error responses only add headers, so have no body.
    for where, resp, doc in all_responses():
        status = where.split()[-1]
        resp = resolve(resp, doc)
        if status.isdigit() and "content" in resp:
            assert is_error(resp) == (int(status) >= 400), where


def test_list_examples_count_their_entries():
    for doc in DOCS.values():
        for name, example in list_example(doc).items():
            assert example["value"]["num_institutions"] == len(example["value"]["institutions"]), name


# Schemas

GOOD = {
    "id": "a",
    "name": "A",
    "provider": "globus",
    "redirect_idp_domain": "a.edu",
    "idp_logout_url": "https://idp.a.edu/logout",
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
        ("provider", "Not-A-Value"),
    ],
)
def test_bad_entries_are_refused(field, bad):
    assert errors("Institution", {**GOOD, field: bad}), f"{field} = {bad!r}"


@pytest.mark.parametrize("field", ["id", "name", "provider", "redirect_idp_domain", "idp_logout_url"])
def test_every_field_is_required(field):
    assert errors("Institution", {k: v for k, v in GOOD.items() if k != field}), field


def test_linked_types_are_checked():
    good = {"type": "ItemList", "institutions": [{"type": "Organization"}]}
    assert not errors("LinkedList", good, ADDENDUM)
    assert errors("LinkedList", {**good, "type": "Thing"}, ADDENDUM)
    assert errors("LinkedList", {**good, "institutions": [{"type": "Thing"}]}, ADDENDUM)


def test_tacc_tapis_entries_have_no_domain():
    assert errors("Institution", {**GOOD, "provider": "tacc_tapis"})
    assert not errors("Institution", {**GOOD, "provider": "tacc_tapis", "redirect_idp_domain": None})


def test_unknown_providers_are_accepted():
    # Provider is an open set: a later version may add values.
    assert not errors("Institution", {**GOOD, "provider": "some_future_flow"})


def test_data_version_is_etag_safe():
    def response(version):
        return {"result_code": "0", "result_msg": "", "data_version": version, "num_institutions": 0, "institutions": []}

    assert not errors("InstitutionsResponse", response("2026-10-23.1"))
    for bad in ["a,b", 'a"b', "a b", "", "a" * 65]:
        assert errors("InstitutionsResponse", response(bad)), repr(bad)


def test_etag_is_weak():
    etag = jsonschema.Draft202012Validator(COMPONENTS["headers"]["ETag"]["schema"])
    for good in ['W/"2026-10-23.1"', f'W/"2026-10-23.1;r{LATEST}"']:
        assert etag.is_valid(good), good
    for bad in ['"2026-10-23.1"', 'W/""', 'W/"a b"', 'W/"a"b"', "W/2026"]:
        assert not etag.is_valid(bad), bad


# Spec and context agree


def test_every_field_has_a_term_in_the_context():
    ctx = latest_context()
    fields = set(SCHEMAS["InstitutionsResponse"]["properties"]) | set(SCHEMAS["Institution"]["properties"])
    fields |= set(ADDENDUM["components"]["schemas"]["LinkedList"]["properties"])
    for field in fields:
        assert field in ctx, f"{field} must be mapped (or null) in the context"
    for schema in ("LinkedList", "LinkedEntry"):
        type_name = ADDENDUM["components"]["schemas"][schema]["properties"]["type"]["const"]
        assert isinstance(ctx.get(type_name), str), f"{schema} type {type_name}"


def test_only_the_linked_envelope_fields_are_in_the_graph():
    ctx = latest_context()
    linked = set(ADDENDUM["components"]["schemas"]["LinkedList"]["properties"]) - {"type"}
    assert linked <= set(SCHEMAS["InstitutionsResponse"]["properties"])
    for field in SCHEMAS["InstitutionsResponse"]["properties"]:
        if field in linked:
            assert ctx[field] is not None, f"{field} is in the linked-data view, so the context maps it"
        else:
            assert ctx[field] is None, f"{field} describes the response, so the context leaves it out"


def test_the_context_maps_no_field_the_spec_lacks():
    ctx = latest_context()
    fields = set(SCHEMAS["InstitutionsResponse"]["properties"]) | set(SCHEMAS["Institution"]["properties"]) | {"type"}
    types = {ADDENDUM["components"]["schemas"][s]["properties"]["type"]["const"] for s in ("LinkedList", "LinkedEntry")}
    prefixes = {k for k, v in ctx.items() if isinstance(v, str) and v.endswith(("/", "#"))}
    extra = set(ctx) - fields - types - prefixes - set(SCHEMAS["Provider"]["x-known-values"])
    assert not extra, f"context terms for no field: {sorted(extra)}"


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


def test_addendum_publishes_each_revisions_hash():
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
    base = ADDENDUM["servers"][0]["variables"]["base_url"]["default"]
    expected = f"<{base}{CONTEXT.format(revision=LATEST)}>; {LINK_REL}"
    assert ADDENDUM["components"]["headers"]["ContextLink"]["example"] == expected


def test_addendum_names_only_the_latest_revision():
    context_prefix = CONTEXT.split("{")[0]
    mentions = [int(m) for m in re.findall(re.escape(context_prefix) + r"(\d+)", ADDENDUM_TEXT)]
    assert mentions, "the addendum names the latest revision"
    assert set(mentions) == {LATEST}, f"the addendum names {sorted(set(mentions))}, not only the latest r{LATEST}"


# Both documents


def test_addendum_adds_to_the_core_list_and_nothing_else_there():
    assert set(SPEC["paths"]) == {LIST}
    assert set(ADDENDUM["paths"]) == {LIST, CONTEXT}
    core, extra = SPEC["paths"][LIST], ADDENDUM["paths"][LIST]
    assert set(extra["get"]["responses"]) == set(core["get"]["responses"]), "the addendum covers each core response"
    assert extra["get"]["operationId"] == core["get"]["operationId"]


def test_versions_match():
    assert SPEC["info"]["version"] == ADDENDUM["info"]["version"]


def test_every_response_forbids_sniffing():
    for where, resp, doc in all_responses():
        headers = {name.lower(): resolve(h, doc) for name, h in resolve(resp, doc).get("headers", {}).items()}
        assert headers.get("x-content-type-options") is COMPONENTS["headers"]["NoSniff"], where


def test_errors_are_never_stored():
    for where, _, resp, doc in error_responses():
        assert resolve(resp["headers"]["Cache-Control"], doc) is COMPONENTS["headers"]["ErrorCacheControl"], where


def test_error_schema_is_tms_servers_http_result():
    assert errors("HttpResult", {"result_code": "500", "result_msg": "ERROR: x"}) == []
    for bad in [{"result_code": 500, "result_msg": "x"}, {"result_code": "200", "result_msg": "x"}, {"result_code": "500"}, {"result_code": "500", "result_msg": ""}]:
        assert errors("HttpResult", bad), bad


# Header names whose values are comma-separated lists (RFC 9110 §5.6.1).
LIST_HEADERS = {"cache-control", "vary", "allow", "access-control-allow-methods", "access-control-allow-headers", "access-control-expose-headers"}


def all_headers():
    """(where, name, header) for every response header either document defines, refs resolved."""
    for where, resp, doc in all_responses():
        for name, h in resolve(resp, doc).get("headers", {}).items():
            yield f"{where} {name}", name.lower(), resolve(h, doc)


def test_list_headers_name_members_not_an_exact_value():
    checked = 0
    for where, name, header in all_headers():
        if name in LIST_HEADERS:
            # A const would refuse the members a deployment may add.
            assert "const" not in header["schema"], where
            members = header["x-required-members"]
            assert members and [m.strip() for m in header["example"].split(",")] == members, where
            checked += 1
    assert checked >= 10, f"list headers checked: {checked}"


def test_extended_list_values_are_valid():
    for where, name, header in all_headers():
        if name in LIST_HEADERS:
            extended = f"{header['example']}, x-deployment-extra"
            assert jsonschema.Draft202012Validator(header["schema"]).is_valid(extended), where
