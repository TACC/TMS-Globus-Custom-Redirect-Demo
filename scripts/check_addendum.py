"""Checks api/openapi.addendum.yaml, the optional CORS and linked-data
addendum, against the core spec and the JSON-LD context files, with no
implementation involved: its example is the core's with `type` fields added,
it follows the rules every document of the API follows, every field has a
term (or null) in the latest context, and the context revisions are
bookkept consistently (files, enum, hashes, and every place the addendum
names a revision).

    pip install -r conformance/requirements.txt
    python -m pytest scripts/check_addendum.py    # API_SPEC=<path> for another copy

It needs the addendum and the context files next to the spec, and
scripts/check_spec.py next to it for the shared helpers. A deployment that
serves only the core needs neither this file nor the documents it checks.
"""

import hashlib
import re

import yaml

from check_spec import (
    LIST,
    SCHEMAS,
    SPEC,
    SPEC_PATH,
    check_bodies,
    check_error_examples,
    check_errors_not_stored,
    check_list_headers,
    check_no_sniff,
    errors,
    list_example,
)

API_DIR = SPEC_PATH.parent
ADDENDUM_PATH = API_DIR / "openapi.addendum.yaml"
ADDENDUM_NAME = ADDENDUM_PATH.name
ADDENDUM_TEXT = ADDENDUM_PATH.read_text()
ADDENDUM = yaml.safe_load(ADDENDUM_TEXT)

CONTEXT = next(p for p in ADDENDUM["paths"] if "{revision}" in p)
CONTEXT_GET = ADDENDUM["paths"][CONTEXT]["get"]
REVISIONS = ADDENDUM["components"]["parameters"]["Revision"]["schema"]["enum"]
LATEST = max(int(r) for r in REVISIONS)
LINK_REL = 'rel="http://www.w3.org/ns/json-ld#context"; type="application/ld+json"'


def context_file(revision):
    return API_DIR / f"institutions.context.r{revision}.jsonld"


def latest_context():
    return yaml.safe_load(context_file(LATEST).read_text())["@context"]


# Examples


def test_addendum_example_is_the_core_example_with_types():
    for name, example in list_example(ADDENDUM).items():
        assert not errors("InstitutionsResponse", example["value"]), name
        assert not errors("LinkedList", example["value"], ADDENDUM), name
        core = list_example(SPEC)[name]["value"]
        untyped = {k: v for k, v in example["value"].items() if k != "type"}
        untyped["institutions"] = [{k: v for k, v in e.items() if k != "type"} for e in untyped["institutions"]]
        assert untyped == core, name


def test_error_examples_conform():
    checked = check_error_examples(ADDENDUM_NAME, ADDENDUM)
    assert checked >= 1, f"error examples checked: {checked}"


def test_a_body_is_an_http_result_exactly_when_it_is_an_error():
    # The addendum's list error responses only add headers, so have no body.
    assert check_bodies(ADDENDUM_NAME, ADDENDUM)


# Schemas


def test_linked_types_are_checked():
    good = {"type": "ItemList", "institutions": [{"type": "Organization"}]}
    assert not errors("LinkedList", good, ADDENDUM)
    assert errors("LinkedList", {**good, "type": "Thing"}, ADDENDUM)
    assert errors("LinkedList", {**good, "institutions": [{"type": "Thing"}]}, ADDENDUM)


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
    pattern = re.compile(SCHEMAS["Provider"]["pattern"])
    for value in SCHEMAS["Provider"]["x-known-values"]:
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
    assert set(ADDENDUM["paths"]) == {LIST, CONTEXT}
    core, extra = SPEC["paths"][LIST], ADDENDUM["paths"][LIST]
    assert set(extra["get"]["responses"]) == set(core["get"]["responses"]), "the addendum covers each core response"
    assert extra["get"]["operationId"] == core["get"]["operationId"]


def test_versions_match():
    assert SPEC["info"]["version"] == ADDENDUM["info"]["version"]


def test_every_response_forbids_sniffing():
    assert check_no_sniff(ADDENDUM_NAME, ADDENDUM)


def test_errors_are_never_stored():
    assert check_errors_not_stored(ADDENDUM_NAME, ADDENDUM)


def test_list_headers_name_members_not_an_exact_value():
    checked = check_list_headers(ADDENDUM_NAME, ADDENDUM)
    assert checked >= 16, f"list headers checked: {checked}"
