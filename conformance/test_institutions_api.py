"""Black-box conformance tests for any deployment of api/openapi.yaml.

Point them at a running server, in any language, and tell them its CORS
setting (the list of allowed origins: empty, `*`, or comma-separated
origins):

    pip install -r conformance/requirements.txt
    API_BASE_URL=https://tms.example.org API_CORS='https://portal.example.org' \\
        python -m pytest conformance

API_BASE_URL is the deployment's base URL with no trailing slash, exactly
as its Link header gives it. API_SPEC points at the spec to test against
(default: api/openapi.yaml in this repository); the suite needs nothing
else from the repository, so it can be copied next to any copy of the spec.
Every expected value comes from the spec, not from any implementation. The
500 and 503 responses cannot be caused from outside, so they are left to
the server's own tests.
"""

import hashlib
import http
import http.client
import json
import os
import pathlib
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
_cors = [o.strip() for o in os.environ.get("API_CORS", "").split(",") if o.strip()]
# None: same origin only; "*": any origin; otherwise the allowed origins.
CORS = None if not _cors else "*" if _cors == ["*"] else _cors

FORMAT_CHECKER = jsonschema.Draft202012Validator.FORMAT_CHECKER
_formats = {"uri", "uri-reference", "hostname"} - set(FORMAT_CHECKER.checkers)
if _formats:
    raise pytest.UsageError(f"jsonschema cannot check {sorted(_formats)}: pip install -r conformance/requirements.txt")

LIST = "/api/v1/institutions"
CONTEXT = "/api/v1/institutions.context.r{revision}.jsonld"
CONTEXT_GET = SPEC["paths"][CONTEXT]["get"]
REVISIONS = [int(r) for r in SPEC["components"]["parameters"]["Revision"]["schema"]["enum"]]
LATEST = max(REVISIONS)
CONTEXT_SHA256 = CONTEXT_GET["x-context-sha256"]
UNLISTED = "https://unlisted.invalid"
# Origins to send: none, one the setting does not list, and a listed one.
ORIGINS = [None, UNLISTED] + ([CORS[0]] if isinstance(CORS, list) else [])
# A tag no deployment's data will have.
NO_SUCH_TAG = f'"conformance-{uuid.uuid4().hex}"'


def const(header):
    return str(header["schema"]["const"])


def header_const(name):
    return const(SPEC["components"]["headers"][name])


PREFLIGHT_HEADERS = SPEC["paths"][LIST]["options"]["responses"]["204"]["headers"]
LIST_ALLOW = const(PREFLIGHT_HEADERS["Allow"])


def validator(name):
    schema = {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": f"#/components/schemas/{name}",
        "components": SPEC["components"],
    }
    return jsonschema.Draft202012Validator(schema, format_checker=FORMAT_CHECKER)


def assert_valid(name, value):
    errors = [f"{e.message} at {list(e.absolute_path)}" for e in validator(name).iter_errors(value)]
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
        conn.request(method, url.path + path, headers=headers or {})
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


# Header values are compared as the spec's "Header values" section says:
# list members in any order, header names case-insensitively, and
# Cache-Control as directives the response must at least carry.


def members(value, names=False):
    items = {m.strip() for m in (value or "").split(",") if m.strip()}
    return {m.lower() for m in items} if names else items


def assert_members(resp, header, expected, names=False):
    if expected is None:
        assert resp.header(header) is None, header
    else:
        assert resp.header(header) is not None, header
        assert members(resp.header(header), names) == members(expected, names), header


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
    assert_members(resp, "access-control-expose-headers", header_const("CorsExposeHeaders") if allow else None, names=True)
    assert ("origin" in members(resp.header("vary"), names=True)) == isinstance(CORS, list)
    assert resp.header("access-control-allow-credentials") is None
    if CORS is None:
        assert not [n for n in resp.headers if n.startswith("access-control-")]


def assert_problem(resp, status):
    assert resp.status == status
    assert resp.media_type() == "application/problem+json"
    assert_cache_control(resp, header_const("ErrorCacheControl"))
    body = resp.json()
    assert_valid("Problem", body)
    assert body["status"] == status
    assert body["title"] == http.HTTPStatus(status).phrase, "title is the status phrase"


@pytest.fixture(scope="module")
def listing():
    return call("GET", LIST)


def test_list_conforms(listing):
    assert listing.status == 200
    assert listing.media_type() == "application/json"
    assert_cache_control(listing, header_const("ListCacheControl"))
    assert listing.header("link") == expected_link()
    body = listing.json()
    assert_valid("InstitutionsResponse", body)
    ids = [i["id"] for i in body["result"]]
    assert len(ids) == len(set(ids)), "each id at most once"
    if "version" in body:
        assert listing.header("etag") == f'W/"{body["version"]};r{LATEST}"'
        assert jsonschema.Draft202012Validator(SPEC["components"]["headers"]["ETag"]["schema"]).is_valid(listing.header("etag"))
    else:
        assert listing.header("etag") is None


def test_order_is_stable(listing):
    assert call("GET", LIST).json()["result"] == listing.json()["result"]


def test_query_and_accept_are_ignored(listing):
    for path, headers in [(f"{LIST}?format=xml&conformance=1", {}), (LIST, {"Accept": "application/xml"})]:
        resp = call("GET", path, headers)
        assert resp.status == 200, path
        assert resp.media_type() == "application/json"
        assert resp.json()["result"] == listing.json()["result"]
    resp = call("GET", context_path(LATEST) + "?conformance=1", {"Accept": "text/html"})
    assert resp.status == 200
    assert resp.media_type() == "application/ld+json"


def assert_head_matches_get(path, headers=None):
    got = call("GET", path, headers)
    head = call("HEAD", path, headers)
    assert head.status == got.status, path
    assert head.body == b"", path
    for name in set(got.headers) - {"date", "content-length", "transfer-encoding", "connection", "keep-alive"}:
        assert head.header(name) == got.header(name), f"{path}: {name}"


@pytest.mark.parametrize("origin", ORIGINS)
def test_head_matches_get(listing, origin):
    assert_head_matches_get(LIST, with_origin(origin))
    assert_head_matches_get(LIST, with_origin(origin, **{"If-None-Match": listing.header("etag") or "*"}))
    for revision in REVISIONS:
        assert_head_matches_get(context_path(revision), with_origin(origin))
    assert_head_matches_get("/api/v1/conformance-no-such-path", with_origin(origin))


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
        assert_cache_control(resp, header_const("ListCacheControl"))
        assert resp.header("etag") == etag, tag
        assert resp.header("link") is None, tag
        assert resp.header("content-type") is None, tag


def test_other_if_none_match_gets_the_list(listing):
    tags = [NO_SUCH_TAG, "W/" + NO_SUCH_TAG, f"{NO_SUCH_TAG}, *", f"*, {NO_SUCH_TAG}"]
    if etag := listing.header("etag"):
        opaque = etag.removeprefix("W/").strip('"')
        # Not entity tags: unquoted, lower-case w/, a space after W/.
        tags += [opaque, "w/" + etag.removeprefix("W/"), "W/ " + etag.removeprefix("W/")]
        # Only the data's version, as if the context had not moved.
        tags.append(f'W/"{opaque.rsplit(";r", 1)[0]}"')
    for tag in tags:
        assert call("GET", LIST, {"If-None-Match": tag}).status == 200, tag


def test_if_match_passes_only_as_any(listing):
    assert call("GET", LIST, {"If-Match": "*"}).status == 200
    for tag in filter(None, [listing.header("etag"), (listing.header("etag") or "").removeprefix("W/"), NO_SUCH_TAG]):
        assert_problem(call("GET", LIST, {"If-Match": tag, "If-None-Match": "*"}), 412)
    assert call("GET", context_path(LATEST), {"If-Match": "*"}).status == 200
    resp = call("GET", context_path(LATEST), {"If-Match": NO_SUCH_TAG})
    assert_problem(resp, 412)
    assert resp.header("access-control-allow-origin") == header_const("AnyOrigin")


@pytest.mark.parametrize("origin", ORIGINS)
def test_list_follows_the_cors_setting(origin):
    assert_list_cors(call("GET", LIST, with_origin(origin)), origin)
    not_modified = call("GET", LIST, with_origin(origin, **{"If-None-Match": "*"}))
    assert not_modified.status == 304
    assert_list_cors(not_modified, origin)
    rejected = call("POST", LIST, with_origin(origin))
    assert rejected.status == 405
    assert_list_cors(rejected, origin)
    failed = call("GET", LIST, with_origin(origin, **{"If-Match": NO_SUCH_TAG}))
    assert failed.status == 412
    assert_list_cors(failed, origin)


@pytest.mark.parametrize("origin", ORIGINS)
def test_preflight(origin):
    resp = call(
        "OPTIONS",
        LIST,
        with_origin(origin, **{"Access-Control-Request-Method": "GET", "Access-Control-Request-Headers": "if-none-match"}),
    )
    assert resp.status == 204
    assert resp.body == b""
    assert_members(resp, "allow", LIST_ALLOW)
    allow = allowed(origin)
    assert resp.header("access-control-allow-origin") == allow
    assert_members(resp, "access-control-allow-methods", const(PREFLIGHT_HEADERS["Access-Control-Allow-Methods"]) if allow else None)
    assert_members(
        resp, "access-control-allow-headers", const(PREFLIGHT_HEADERS["Access-Control-Allow-Headers"]) if allow else None, names=True
    )
    assert resp.header("access-control-max-age") == (const(PREFLIGHT_HEADERS["Access-Control-Max-Age"]) if allow else None)
    assert ("origin" in members(resp.header("vary"), names=True)) == isinstance(CORS, list)
    assert resp.header("access-control-allow-credentials") is None
    if not allow:
        assert not [n for n in resp.headers if n.startswith("access-control-")]


@pytest.mark.parametrize("method", ["POST", "PUT", "DELETE", "PATCH"])
def test_list_rejects_other_methods(method):
    resp = call(method, LIST)
    assert_problem(resp, 405)
    assert_members(resp, "allow", LIST_ALLOW)


@pytest.mark.parametrize("revision", REVISIONS)
def test_every_context_revision_is_served(revision):
    resp = call("GET", context_path(revision))
    assert resp.status == 200
    assert resp.media_type() == "application/ld+json"
    assert resp.header("access-control-allow-origin") == header_const("AnyOrigin")
    assert_cache_control(resp, header_const("ContextCacheControl"))
    # Served byte for byte as published.
    assert hashlib.sha256(resp.body).hexdigest() == CONTEXT_SHA256[str(revision)]


def test_latest_context_matches_the_spec_example():
    example = CONTEXT_GET["responses"]["200"]["content"]["application/ld+json"]["examples"]["latest"]["value"]
    assert call("GET", context_path(LATEST)).json() == example


@pytest.mark.parametrize("revision", REVISIONS)
def test_context_conditional_requests(revision):
    resp = call("GET", context_path(revision), {"If-None-Match": "*"})
    assert resp.status == 304
    assert resp.body == b""
    assert resp.header("content-type") is None
    assert resp.header("access-control-allow-origin") == header_const("AnyOrigin")
    assert_cache_control(resp, header_const("ContextCacheControl"))
    # Contexts have no ETag, so no tag matches.
    assert call("GET", context_path(revision), {"If-None-Match": NO_SUCH_TAG}).status == 200


@pytest.mark.parametrize("revision", ["0", "01", "+1", str(LATEST + 1)])
def test_unpublished_revisions_are_not_found(revision):
    resp = call("GET", context_path(revision))
    assert_problem(resp, 404)
    assert resp.header("access-control-allow-origin") == header_const("AnyOrigin")


@pytest.mark.parametrize(
    "path", ["/api/v1", "/api/v1/", f"{LIST}/", "/api/v1/conformance-no-such-path", "/api/v1/conformance/no/such/path"]
)
def test_other_api_paths_are_not_found(path):
    for method in ("GET", "POST"):
        resp = call(method, path)
        assert_problem(resp, 404)
        assert resp.header("access-control-allow-origin") == header_const("AnyOrigin"), f"{method} {path}"


@pytest.mark.parametrize("method", ["POST", "DELETE", "OPTIONS"])
def test_context_rejects_other_methods(method):
    resp = call(method, context_path(LATEST))
    assert_problem(resp, 405)
    assert_members(resp, "allow", "GET, HEAD")
    assert resp.header("access-control-allow-origin") == header_const("AnyOrigin")
