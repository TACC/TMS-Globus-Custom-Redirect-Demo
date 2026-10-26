"""Checks api/openapi.yaml, the core of the API, against itself, with no
implementation involved: the examples fit their schemas, the schemas refuse
what they should, and the response headers follow the document's rules.

    pip install -r conformance/requirements.txt
    python -m pytest scripts/check_spec.py        # API_SPEC=<path> for another copy

It needs only the spec, so it travels with that one file to any
implementation's repository. The optional addendum and its JSON-LD contexts
are checked by scripts/check_addendum.py, which reuses the helpers here.
"""

import os
import pathlib

import jsonschema
import pytest
import yaml

SPEC_PATH = pathlib.Path(os.environ.get("API_SPEC") or pathlib.Path(__file__).resolve().parent.parent / "api/openapi.yaml")
SPEC_NAME = SPEC_PATH.name
SPEC = yaml.safe_load(SPEC_PATH.read_text())
COMPONENTS = SPEC["components"]
SCHEMAS = COMPONENTS["schemas"]
FORMAT_CHECKER = jsonschema.Draft202012Validator.FORMAT_CHECKER

LIST = "/v1/tms/institutions"


def validator(name, doc=SPEC):
    schema = {"$schema": "https://json-schema.org/draft/2020-12/schema", "$ref": f"#/components/schemas/{name}", "components": doc["components"]}
    return jsonschema.Draft202012Validator(schema, format_checker=FORMAT_CHECKER)


def errors(name, value, doc=SPEC):
    return [f"{e.message} at {list(e.absolute_path)}" for e in validator(name, doc).iter_errors(value)]


def resolve(obj, doc):
    """`obj` with its $refs followed, within `doc` or into the core spec (the
    only other document a $ref names)."""
    while "$ref" in obj:
        file, _, pointer = obj["$ref"].partition("#")
        if file:
            doc = SPEC
        *_, kind, name = pointer.split("/")
        obj = doc["components"][kind][name]
    return obj


def all_responses(doc_name, doc):
    """(where, response, doc) for every response `doc` defines."""
    for path, item in doc["paths"].items():
        for method, op in item.items():
            for status, resp in op["responses"].items():
                yield f"{doc_name} {method.upper()} {path} {status}", resp, doc
    for name, resp in doc["components"].get("responses", {}).items():
        yield f"{doc_name} components/responses/{name}", resp, doc


def is_error(resp):
    schema = resp.get("content", {}).get("application/json", {}).get("schema", {})
    return schema.get("$ref", "").endswith("/schemas/HttpResult")


def error_responses(doc_name, doc):
    """(where, status, response, doc) for every response in `doc` whose body
    is an HttpResult; status is None for a component response."""
    for path, item in doc["paths"].items():
        for method, op in item.items():
            for status, resp in op["responses"].items():
                if is_error(resolve(resp, doc)):
                    yield f"{doc_name} {method.upper()} {path} {status}", status, resolve(resp, doc), doc
    for name, resp in doc["components"].get("responses", {}).items():
        if is_error(resp):
            yield f"{doc_name} components/responses/{name}", None, resp, doc


def list_example(doc):
    return doc["paths"][LIST]["get"]["responses"]["200"]["content"]["application/json"]["examples"]


# Header names whose values are comma-separated lists (RFC 9110 §5.6.1).
LIST_HEADERS = {"cache-control", "vary", "allow", "access-control-allow-methods", "access-control-allow-headers", "access-control-expose-headers"}


def all_headers(doc_name, doc):
    """(where, name, header) for every response header `doc` defines, refs resolved."""
    for where, resp, doc in all_responses(doc_name, doc):
        for name, h in resolve(resp, doc).get("headers", {}).items():
            yield f"{where} {name}", name.lower(), resolve(h, doc)


# Rules every document of the API follows; check_addendum.py applies them to
# the addendum. Each returns how many items it checked, so a test can tell
# that it checked something.


def check_error_examples(doc_name, doc):
    checked = 0
    for where, status, resp, _ in error_responses(doc_name, doc):
        examples = resp["content"]["application/json"].get("examples")
        assert examples, f"{where} has an example"
        for example in examples.values():
            value = example["value"]
            assert not errors("HttpResult", value), where
            if status:
                assert value["result_code"] == status, f"{where}: result_code is the status"
            checked += 1
    return checked


def check_bodies(doc_name, doc):
    """A body is an HttpResult exactly when its response is an error."""
    checked = 0
    for where, resp, doc in all_responses(doc_name, doc):
        status = where.split()[-1]
        resp = resolve(resp, doc)
        if status.isdigit() and "content" in resp:
            assert is_error(resp) == (int(status) >= 400), where
            checked += 1
    return checked


def check_no_sniff(doc_name, doc):
    checked = 0
    for where, resp, doc in all_responses(doc_name, doc):
        headers = {name.lower(): resolve(h, doc) for name, h in resolve(resp, doc).get("headers", {}).items()}
        assert headers.get("x-content-type-options") is COMPONENTS["headers"]["NoSniff"], where
        checked += 1
    return checked


def check_errors_not_stored(doc_name, doc):
    checked = 0
    for where, _, resp, doc in error_responses(doc_name, doc):
        assert resolve(resp["headers"]["Cache-Control"], doc) is COMPONENTS["headers"]["ErrorCacheControl"], where
        checked += 1
    return checked


def check_list_headers(doc_name, doc):
    """List-valued headers name the members they carry at least, not an exact
    value, and accept a value a deployment has added to."""
    checked = 0
    for where, name, header in all_headers(doc_name, doc):
        if name in LIST_HEADERS:
            # A const would refuse the members a deployment may add.
            assert "const" not in header["schema"], where
            members = header["x-required-members"]
            assert members and [m.strip() for m in header["example"].split(",")] == members, where
            extended = f"{header['example']}, x-deployment-extra"
            assert jsonschema.Draft202012Validator(header["schema"]).is_valid(extended), where
            checked += 1
    return checked


# Examples


def test_list_examples_conform():
    examples = list_example(SPEC)
    assert examples
    for name, example in examples.items():
        assert not errors("InstitutionsResponse", example["value"]), name
        assert "mock" not in name, f"example {name!r} names the demo"


def test_list_examples_count_their_entries():
    for name, example in list_example(SPEC).items():
        assert example["value"]["num_institutions"] == len(example["value"]["institutions"]), name


def test_error_examples_conform():
    checked = check_error_examples(SPEC_NAME, SPEC)
    assert checked >= 4, f"error examples checked: {checked}"


def test_a_body_is_an_http_result_exactly_when_it_is_an_error():
    assert check_bodies(SPEC_NAME, SPEC)


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


def test_tacc_tapis_entries_have_no_domain():
    assert errors("Institution", {**GOOD, "provider": "tacc_tapis"})
    assert not errors("Institution", {**GOOD, "provider": "tacc_tapis", "redirect_idp_domain": None})


def test_unknown_providers_are_accepted():
    # Provider is an open set: a later version may add values.
    assert not errors("Institution", {**GOOD, "provider": "some_future_flow"})


def test_known_providers_are_listed():
    known = SCHEMAS["Provider"]["x-known-values"]
    assert known and known == SCHEMAS["Provider"]["examples"]
    for value in known:
        assert not errors("Institution", {**GOOD, "provider": value, "redirect_idp_domain": None}), value


def test_data_version_is_etag_safe():
    def response(version):
        return {"result_code": "0", "result_msg": "", "data_version": version, "num_institutions": 0, "institutions": []}

    assert not errors("InstitutionsResponse", response("2026-10-23.1"))
    for bad in ["a,b", 'a"b', "a b", "", "a" * 65]:
        assert errors("InstitutionsResponse", response(bad)), repr(bad)


def test_etag_is_weak():
    etag = jsonschema.Draft202012Validator(COMPONENTS["headers"]["ETag"]["schema"])
    for good in ['W/"2026-10-23.1"', 'W/"2026-10-23.1;r1"', 'W/"5d2c67a227cb9f41baba858a74b8a3e9"']:
        assert etag.is_valid(good), good
    for bad in ['"2026-10-23.1"', 'W/""', 'W/"a b"', 'W/"a"b"', "W/2026"]:
        assert not etag.is_valid(bad), bad


def test_error_schema_is_tms_servers_http_result():
    assert errors("HttpResult", {"result_code": "500", "result_msg": "ERROR: x"}) == []
    for bad in [{"result_code": 500, "result_msg": "x"}, {"result_code": "200", "result_msg": "x"}, {"result_code": "500"}, {"result_code": "500", "result_msg": ""}]:
        assert errors("HttpResult", bad), bad


# The document


def test_the_list_is_the_only_path():
    assert set(SPEC["paths"]) == {LIST}
    assert set(SPEC["paths"][LIST]) == {"get"}


def test_every_response_forbids_sniffing():
    assert check_no_sniff(SPEC_NAME, SPEC)


def test_errors_are_never_stored():
    checked = check_errors_not_stored(SPEC_NAME, SPEC)
    assert checked >= 4, f"error responses checked: {checked}"


def test_list_headers_name_members_not_an_exact_value():
    checked = check_list_headers(SPEC_NAME, SPEC)
    assert checked >= 7, f"list headers checked: {checked}"
