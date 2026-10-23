"""Black-box conformance tests for any deployment of api/openapi.yaml.

Point them at a running server, in any language, and tell them its CORS
setting (the list of allowed origins: empty, `*`, or comma-separated
origins):

    pip install -r conformance/requirements.txt
    API_BASE_URL=https://tms.example.org API_CORS='https://portal.example.org' \\
        python -m pytest conformance

API_BASE_URL is the deployment's base URL with no trailing slash, exactly
as its Link header gives it. The expected values come from the spec and the
context files next to it, not from any implementation. The 500 and 503
responses cannot be caused from outside, so they are left to the server's
own tests.
"""

import http.client
import json
import os
import pathlib
import urllib.parse

import jsonschema
import pytest
import yaml

ROOT = pathlib.Path(__file__).resolve().parent.parent
SPEC = yaml.safe_load((ROOT / "api/openapi.yaml").read_text())

BASE = os.environ.get("API_BASE_URL", "")
if not BASE or BASE.endswith("/"):
    raise pytest.UsageError("Set API_BASE_URL to the deployment's base URL, with no trailing slash.")
_cors = [o.strip() for o in os.environ.get("API_CORS", "").split(",") if o.strip()]
# None: same origin only; "*": any origin; otherwise the allowed origins.
CORS = None if not _cors else "*" if _cors == ["*"] else _cors

LIST = "/api/v1/institutions"
CONTEXT = "/api/v1/institutions.context.r{revision}.jsonld"
CONTEXT_GET = SPEC["paths"][CONTEXT]["get"]
REVISIONS = CONTEXT_GET["parameters"][0]["schema"]["enum"]
LATEST = max(REVISIONS)
UNLISTED = "https://unlisted.invalid"
# Origins to send: none, one the setting does not list, and a listed one.
ORIGINS = [None, UNLISTED] + ([CORS[0]] if isinstance(CORS, list) else [])
NO_SUCH_TAG = '"conformance-no-such-version"'


def spec_const(*path):
    node = SPEC
    for key in path:
        node = node[key]
    return str(node["schema"]["const"])


def header_const(name):
    return spec_const("components", "headers", name)


PREFLIGHT_HEADERS = SPEC["paths"][LIST]["options"]["responses"]["204"]["headers"]
CONTEXT_HEADERS = CONTEXT_GET["responses"]["200"]["headers"]


def validator(name):
    schema = {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": f"#/components/schemas/{name}",
        "components": SPEC["components"],
    }
    return jsonschema.Draft202012Validator(schema, format_checker=jsonschema.Draft202012Validator.FORMAT_CHECKER)


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


def vary_tokens(resp):
    return {t.strip().lower() for t in (resp.header("vary") or "").split(",") if t.strip()}


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
    assert resp.header("access-control-expose-headers") == (header_const("CorsExposeHeaders") if allow else None)
    assert ("origin" in vary_tokens(resp)) == isinstance(CORS, list)
    assert resp.header("access-control-allow-credentials") is None
    if CORS is None:
        assert not [n for n in resp.headers if n.startswith("access-control-")]


def assert_problem(resp, status):
    assert resp.status == status
    assert resp.media_type() == "application/problem+json"
    assert resp.header("cache-control") == header_const("ErrorCacheControl")
    body = resp.json()
    assert_valid("Problem", body)
    assert body["status"] == status


@pytest.fixture(scope="module")
def listing():
    return call("GET", LIST)


def test_list_conforms(listing):
    assert listing.status == 200
    assert listing.media_type() == "application/json"
    assert listing.header("cache-control") == header_const("ListCacheControl")
    assert listing.header("link") == expected_link()
    body = listing.json()
    assert_valid("InstitutionsResponse", body)
    ids = [i["id"] for i in body["result"]]
    assert len(ids) == len(set(ids)), "each id at most once"
    if "version" in body:
        assert listing.header("etag") == f'W/"{body["version"]}"'
        assert jsonschema.Draft202012Validator(SPEC["components"]["headers"]["ETag"]["schema"]).is_valid(listing.header("etag"))
    else:
        assert listing.header("etag") is None


def test_head_matches_get(listing):
    resp = call("HEAD", LIST)
    assert resp.status == 200
    assert resp.body == b""
    for name in ("content-type", "cache-control", "etag", "link"):
        assert resp.header(name) == listing.header(name), name


def matching_tags(etag):
    if not etag:
        return ["*"]
    strong = etag.removeprefix("W/")
    return ["*", etag, strong, f"{NO_SUCH_TAG}, {etag}"]


def test_matching_if_none_match_gets_not_modified(listing):
    etag = listing.header("etag")
    for tag in matching_tags(etag):
        resp = call("GET", LIST, {"If-None-Match": tag})
        assert resp.status == 304, tag
        assert resp.body == b"", tag
        assert resp.header("cache-control") == header_const("ListCacheControl"), tag
        assert resp.header("etag") == etag, tag
        assert resp.header("link") is None, tag


def test_other_if_none_match_gets_the_list():
    for tag in (NO_SUCH_TAG, "W/" + NO_SUCH_TAG):
        assert call("GET", LIST, {"If-None-Match": tag}).status == 200, tag


@pytest.mark.parametrize("origin", ORIGINS)
def test_list_follows_the_cors_setting(origin):
    assert_list_cors(call("GET", LIST, with_origin(origin)), origin)
    not_modified = call("GET", LIST, with_origin(origin, **{"If-None-Match": "*"}))
    assert not_modified.status == 304
    assert_list_cors(not_modified, origin)
    rejected = call("POST", LIST, with_origin(origin))
    assert rejected.status == 405
    assert_list_cors(rejected, origin)


@pytest.mark.parametrize("origin", ORIGINS)
def test_preflight(origin):
    resp = call(
        "OPTIONS",
        LIST,
        with_origin(origin, **{"Access-Control-Request-Method": "GET", "Access-Control-Request-Headers": "if-none-match"}),
    )
    assert resp.status == 204
    assert resp.body == b""
    assert resp.header("allow") == str(PREFLIGHT_HEADERS["Allow"]["schema"]["const"])
    allow = allowed(origin)
    assert resp.header("access-control-allow-origin") == allow
    for name in ("Access-Control-Allow-Methods", "Access-Control-Allow-Headers", "Access-Control-Max-Age"):
        expected = str(PREFLIGHT_HEADERS[name]["schema"]["const"]) if allow else None
        assert resp.header(name) == expected, name
    assert ("origin" in vary_tokens(resp)) == isinstance(CORS, list)
    assert resp.header("access-control-allow-credentials") is None
    if not allow:
        assert not [n for n in resp.headers if n.startswith("access-control-")]


@pytest.mark.parametrize("method", ["POST", "PUT", "DELETE", "PATCH"])
def test_list_rejects_other_methods(method):
    resp = call(method, LIST)
    assert_problem(resp, 405)
    assert resp.header("allow") == str(PREFLIGHT_HEADERS["Allow"]["schema"]["const"])


@pytest.mark.parametrize("revision", REVISIONS)
def test_every_context_revision_is_served(revision):
    resp = call("GET", context_path(revision))
    assert resp.status == 200
    assert resp.media_type() == "application/ld+json"
    assert resp.header("access-control-allow-origin") == header_const("AnyOrigin")
    assert resp.header("cache-control") == str(CONTEXT_HEADERS["Cache-Control"]["schema"]["const"])
    # The files next to the spec are the authoritative content.
    assert resp.json() == json.loads((ROOT / f"api/institutions.context.r{revision}.jsonld").read_text())


def test_latest_context_matches_the_spec_example():
    example = CONTEXT_GET["responses"]["200"]["content"]["application/ld+json"]["examples"]["latest"]["value"]
    assert call("GET", context_path(LATEST)).json() == example


@pytest.mark.parametrize("revision", ["0", "01", str(LATEST + 1)])
def test_unpublished_revisions_are_not_found(revision):
    resp = call("GET", context_path(revision))
    assert_problem(resp, 404)
    assert resp.header("access-control-allow-origin") == header_const("AnyOrigin")


def test_other_api_paths_are_not_found():
    assert_problem(call("GET", "/api/v1/conformance-no-such-path"), 404)


@pytest.mark.parametrize("method", ["POST", "DELETE"])
def test_context_rejects_other_methods(method):
    resp = call(method, context_path(LATEST))
    assert_problem(resp, 405)
    assert resp.header("allow") == "GET, HEAD"
    assert resp.header("access-control-allow-origin") == header_const("AnyOrigin")
