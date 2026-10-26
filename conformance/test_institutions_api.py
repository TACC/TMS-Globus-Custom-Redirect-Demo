"""Black-box conformance tests for any deployment of api/openapi.yaml.

Point them at a running server, in any language:

    pip install -r conformance/requirements.txt
    API_BASE_URL=https://tms.example.org python -m pytest conformance

API_BASE_URL is the deployment's base URL with no trailing slash. API_SPEC
points at the spec to test against (default: api/openapi.yaml in this
repository), so the suite can be copied next to any copy of the spec. With
API_ADDENDUM set, the addendum is read from next to the spec as well; a
core-only copy needs no other file. API_EXTRA_HEADERS, a JSON
object, adds headers to every request, for example the credentials a
gateway in front of a staging server needs.

The core spec is always tested. A deployment that also serves the optional
addendum (openapi.addendum.yaml: CORS and linked data) sets API_ADDENDUM=1
and API_CORS to its CORS setting, the list of allowed origins: empty, `*`,
or comma-separated origins, exactly as it allows them.

Every expected value comes from the spec, not from any implementation. The
500 and 503 responses, and the other requirements that need control of the
data, cannot be tested from outside: SERVER_TESTS.md lists them for the
server's own tests.
"""

import hashlib
import http.client
import json
import os
import pathlib
import re
import urllib.parse
import uuid

import jsonschema
import pytest
import yaml

SPEC_PATH = pathlib.Path(os.environ.get("API_SPEC") or pathlib.Path(__file__).resolve().parent.parent / "api/openapi.yaml")
SPEC = yaml.safe_load(SPEC_PATH.read_text())

BASE = os.environ.get("API_BASE_URL", "")
if not BASE or BASE.endswith("/"):
    raise pytest.UsageError("Set API_BASE_URL to the deployment's base URL, with no trailing slash.")
ADDENDUM = os.environ.get("API_ADDENDUM", "").lower() in ("1", "true", "yes")
_cors = [o.strip() for o in os.environ.get("API_CORS", "").split(",") if o.strip()]
# None: same origin only; "*": any origin; otherwise the allowed origins.
CORS = None if not _cors else "*" if _cors == ["*"] else _cors
try:
    EXTRA_HEADERS = json.loads(os.environ.get("API_EXTRA_HEADERS") or "{}")
except json.JSONDecodeError as e:
    raise pytest.UsageError(f"API_EXTRA_HEADERS must be a JSON object of header names to values: {e}")

FORMAT_CHECKER = jsonschema.Draft202012Validator.FORMAT_CHECKER
_formats = {"uri", "uri-reference", "hostname"} - set(FORMAT_CHECKER.checkers)
if _formats:
    raise pytest.UsageError(f"jsonschema cannot check {sorted(_formats)}: pip install -r conformance/requirements.txt")

addendum = pytest.mark.skipif(not ADDENDUM, reason="the deployment does not serve the addendum (API_ADDENDUM)")

LIST = "/v1/tms/institutions"
if ADDENDUM:
    ADDENDUM_SPEC = yaml.safe_load((SPEC_PATH.parent / "openapi.addendum.yaml").read_text())
    CONTEXT = next(p for p in ADDENDUM_SPEC["paths"] if "{revision}" in p)
    CONTEXT_GET = ADDENDUM_SPEC["paths"][CONTEXT]["get"]
    REVISIONS = [int(r) for r in ADDENDUM_SPEC["components"]["parameters"]["Revision"]["schema"]["enum"]]
    LATEST = max(REVISIONS)
    # Revisions that were never published.
    UNPUBLISHED = ["0", "01", "+1", f"r{LATEST}", str(LATEST + 1)]
else:
    # Core only: the addendum need not be next to the spec, and its tests are skipped.
    ADDENDUM_SPEC = CONTEXT = CONTEXT_GET = LATEST = None
    REVISIONS = UNPUBLISHED = []
UNLISTED = "https://unlisted.invalid"
# Origins to send: none, one the setting does not list, and a listed one.
ORIGINS = [None, UNLISTED] + ([CORS[0]] if isinstance(CORS, list) else [])
# A tag no deployment's data will have.
NO_SUCH_TAG = f'"conformance-{uuid.uuid4().hex}"'


def const(header):
    return str(header["schema"]["const"])


def header_const(name, doc=SPEC):
    return const(doc["components"]["headers"][name])


def addendum_const(name):
    return header_const(name, ADDENDUM_SPEC)


def required(header):
    """The members a list-valued header carries at least, as a list value."""
    return ", ".join(header["x-required-members"])


def header_required(name, doc=SPEC):
    return required(doc["components"]["headers"][name])


def addendum_required(name):
    return header_required(name, ADDENDUM_SPEC)


NO_SNIFF = header_const("NoSniff")


def validator(name, doc=SPEC):
    schema = {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": f"#/components/schemas/{name}",
        "components": doc["components"],
    }
    return jsonschema.Draft202012Validator(schema, format_checker=FORMAT_CHECKER)


def assert_valid(name, value, doc=SPEC):
    errors = [f"{e.message} at {list(e.absolute_path)}" for e in validator(name, doc).iter_errors(value)]
    assert not errors, f"{name}: {errors}"


class Response:
    def __init__(self, status, headers, body):
        self.status = status
        # Lower-case names; repeated headers joined as RFC 9110 allows.
        self.headers = {}
        for name, value in headers:
            name = name.lower()
            self.headers[name] = f"{self.headers[name]}, {value}" if name in self.headers else value
        self.body = body

    def header(self, name):
        return self.headers.get(name.lower())

    def media_type(self):
        return (self.header("content-type") or "").split(";")[0].strip().lower()

    def json(self):
        return json.loads(self.body)


def call(method, path, headers=None):
    url = urllib.parse.urlsplit(BASE)
    conn_class = http.client.HTTPSConnection if url.scheme == "https" else http.client.HTTPConnection
    conn = conn_class(url.netloc, timeout=30)
    try:
        conn.request(method, url.path + path, headers={**EXTRA_HEADERS, **(headers or {})})
        resp = conn.getresponse()
        return Response(resp.status, resp.getheaders(), resp.read())
    finally:
        conn.close()


def with_origin(origin, **headers):
    return {"Origin": origin, **headers} if origin else headers


def context_path(revision):
    return CONTEXT.format(revision=revision)


def expected_link():
    return f'<{BASE}{context_path(LATEST)}>; rel="http://www.w3.org/ns/json-ld#context"; type="application/ld+json"'


def links(value):
    """The links in a Link header, each as written."""
    return [link.strip() for link in re.split(r",\s*(?=<)", value or "") if link.strip()]


# Header values are compared as the "Header values" sections of the spec and
# addendum say: list members in any order, header names case-insensitively,
# Vary and the Access-Control-* lists as members the response must at least
# carry, and Cache-Control as directives it must at least carry.


def members(value, names=False):
    items = {m.strip() for m in (value or "").split(",") if m.strip()}
    return {m.lower() for m in items} if names else items


def assert_includes(resp, header, expected, names=True):
    """`header` carries at least the members of `expected`, or is absent for None."""
    if expected is None:
        assert resp.header(header) is None, header
    else:
        assert resp.header(header) is not None, header
        assert members(expected, names) <= members(resp.header(header), names), f"{header}: {resp.header(header)}"


def assert_cache_control(resp, expected):
    assert members(expected, names=True) <= members(resp.header("cache-control"), names=True), resp.header("cache-control")


def allowed(origin):
    """The Access-Control-Allow-Origin the setting gives `origin`, if any."""
    if CORS == "*":
        return "*"
    if isinstance(CORS, list) and origin in CORS:
        return origin
    return None


def assert_list_cors(resp, origin):
    """The CORS rule every non-preflight response from the list follows."""
    allow = allowed(origin)
    assert resp.header("access-control-allow-origin") == allow
    assert_includes(resp, "access-control-expose-headers", addendum_required("CorsExposeHeaders") if allow else None)
    if isinstance(CORS, list):
        assert "origin" in members(resp.header("vary"), names=True)
    assert resp.header("access-control-allow-credentials") is None
    if CORS is None:
        assert not [n for n in resp.headers if n.startswith("access-control-")]


def assert_error(resp, status):
    """An HttpResult error, as the spec gives it, with the given status."""
    assert resp.status == status
    assert resp.media_type() == "application/json"
    assert resp.header("x-content-type-options") == NO_SNIFF
    assert_cache_control(resp, header_required("ErrorCacheControl"))
    body = resp.json()
    assert_valid("HttpResult", body)
    assert body["result_code"] == str(status), "result_code is the status"


@pytest.fixture(scope="module")
def listing():
    return call("GET", LIST)


# Core: api/openapi.yaml


def test_list_conforms(listing):
    assert listing.status == 200
    assert listing.media_type() == "application/json"
    assert listing.header("x-content-type-options") == NO_SNIFF
    assert_cache_control(listing, header_required("ListCacheControl"))
    body = listing.json()
    assert_valid("InstitutionsResponse", body)
    ids = [i["id"] for i in body["institutions"]]
    assert len(ids) == len(set(ids)), "each id at most once"
    assert body["num_institutions"] == len(ids)
    if "data_version" in body:
        assert jsonschema.Draft202012Validator(SPEC["components"]["headers"]["ETag"]["schema"]).is_valid(listing.header("etag"))
    else:
        assert listing.header("etag") is None


def test_order_is_stable(listing):
    assert call("GET", LIST).json()["institutions"] == listing.json()["institutions"]


def test_query_and_accept_are_ignored(listing):
    for path, headers in [(f"{LIST}?format=xml&conformance=1", {}), (LIST, {"Accept": "application/xml"})]:
        resp = call("GET", path, headers)
        assert resp.status == 200, path
        assert resp.media_type() == "application/json"
        assert resp.json()["institutions"] == listing.json()["institutions"]


def matching_tags(etag):
    if not etag:
        return ["*", " * "]
    strong = etag.removeprefix("W/")
    return ["*", etag, strong, f"{NO_SUCH_TAG}, {etag}", f'not-a-tag, W/"broken, {etag}']


def test_matching_if_none_match_gets_not_modified(listing):
    etag = listing.header("etag")
    for tag in matching_tags(etag):
        resp = call("GET", LIST, {"If-None-Match": tag})
        assert resp.status == 304, tag
        assert resp.body == b"", tag
        assert resp.header("x-content-type-options") == NO_SNIFF, tag
        assert_cache_control(resp, header_required("ListCacheControl"))
        assert resp.header("etag") == etag, tag
        assert resp.header("content-type") is None, tag


def test_other_if_none_match_gets_the_list(listing):
    tags = [NO_SUCH_TAG, "W/" + NO_SUCH_TAG, f"{NO_SUCH_TAG}, *", f"*, {NO_SUCH_TAG}"]
    if etag := listing.header("etag"):
        opaque = etag.removeprefix("W/").strip('"')
        # Not entity tags: unquoted, lower-case w/, a space after W/.
        tags += [opaque, "w/" + etag.removeprefix("W/"), "W/ " + etag.removeprefix("W/")]
    for tag in tags:
        assert call("GET", LIST, {"If-None-Match": tag}).status == 200, tag


def test_if_match_is_ignored(listing):
    etag = listing.header("etag")
    for tag in ['"x"', NO_SUCH_TAG, "*"] + ([etag] if etag else []):
        resp = call("GET", LIST, {"If-Match": tag})
        assert resp.status == 200, tag
        assert resp.json()["institutions"] == listing.json()["institutions"]
        # If-None-Match still counts alongside it.
        assert call("GET", LIST, {"If-Match": tag, "If-None-Match": "*"}).status == 304, tag


# Addendum: api/openapi.addendum.yaml


@addendum
def test_list_is_linked_data(listing):
    assert expected_link() in links(listing.header("link")), listing.header("link")
    assert_valid("LinkedList", listing.json(), ADDENDUM_SPEC)
    not_modified = call("GET", LIST, {"If-None-Match": listing.header("etag") or "*"})
    assert not_modified.status == 304
    assert not_modified.header("link") is None


@addendum
@pytest.mark.parametrize("origin", ORIGINS)
def test_list_follows_the_cors_setting(origin):
    assert_list_cors(call("GET", LIST, with_origin(origin)), origin)
    not_modified = call("GET", LIST, with_origin(origin, **{"If-None-Match": "*"}))
    assert not_modified.status == 304
    assert_list_cors(not_modified, origin)


@addendum
@pytest.mark.parametrize("origin", ORIGINS)
def test_preflight(origin):
    resp = call(
        "OPTIONS",
        LIST,
        with_origin(origin, **{"Access-Control-Request-Method": "GET", "Access-Control-Request-Headers": "if-none-match"}),
    )
    assert resp.status == 204
    assert resp.body == b""
    assert resp.header("x-content-type-options") == NO_SNIFF
    allow = allowed(origin)
    assert resp.header("access-control-allow-origin") == allow
    preflight_headers = ADDENDUM_SPEC["paths"][LIST]["options"]["responses"]["204"]["headers"]
    assert_includes(resp, "access-control-allow-methods", required(preflight_headers["Access-Control-Allow-Methods"]) if allow else None, names=False)
    assert_includes(resp, "access-control-allow-headers", required(preflight_headers["Access-Control-Allow-Headers"]) if allow else None)
    max_age = resp.header("access-control-max-age")
    if allow:
        assert max_age is not None and max_age.isdigit(), max_age
    else:
        assert max_age is None
    if isinstance(CORS, list):
        assert "origin" in members(resp.header("vary"), names=True)
    assert resp.header("access-control-allow-credentials") is None
    if not allow:
        assert not [n for n in resp.headers if n.startswith("access-control-")]


@addendum
@pytest.mark.parametrize("revision", REVISIONS)
def test_every_context_revision_is_served(revision):
    resp = call("GET", context_path(revision))
    assert resp.status == 200
    assert resp.media_type() == "application/ld+json"
    assert resp.header("x-content-type-options") == NO_SNIFF
    assert resp.header("access-control-allow-origin") == addendum_const("AnyOrigin")
    assert_cache_control(resp, addendum_required("ContextCacheControl"))
    # Served byte for byte as published.
    assert hashlib.sha256(resp.body).hexdigest() == CONTEXT_GET["x-context-sha256"][str(revision)]


@addendum
def test_latest_context_matches_the_addendum_example():
    example = CONTEXT_GET["responses"]["200"]["content"]["application/ld+json"]["examples"]["latest"]["value"]
    resp = call("GET", context_path(LATEST) + "?conformance=1", {"Accept": "text/html"})
    assert resp.media_type() == "application/ld+json"
    assert resp.json() == example


@addendum
@pytest.mark.parametrize("revision", REVISIONS)
def test_context_conditional_requests(revision):
    resp = call("GET", context_path(revision), {"If-None-Match": "*"})
    assert resp.status == 304
    assert resp.body == b""
    assert resp.header("content-type") is None
    assert resp.header("x-content-type-options") == NO_SNIFF
    assert resp.header("access-control-allow-origin") == addendum_const("AnyOrigin")
    assert_cache_control(resp, addendum_required("ContextCacheControl"))
    # Contexts have no ETag, so no tag matches.
    assert call("GET", context_path(revision), {"If-None-Match": NO_SUCH_TAG}).status == 200


@addendum
@pytest.mark.parametrize("revision", UNPUBLISHED)
def test_unpublished_revisions_are_not_found(revision):
    resp = call("GET", context_path(revision))
    assert_error(resp, 404)
    assert resp.header("access-control-allow-origin") == addendum_const("AnyOrigin")
