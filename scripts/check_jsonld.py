"""Checks that a list response, read with the JSON-LD context, yields the
RDF the spec promises: a typed schema:ItemList of typed schema:Organizations,
with every field mapped and the envelope left out. What each field must map
to is read from the spec's `x-schema-org` and the context, not listed here.

By default it checks the spec's examples against the context file (next to
the spec) that its Link example names. With --url it checks a live
deployment instead: the list at <base>/api/v1/institutions and the context
its Link header points to. API_EXTRA_HEADERS, a JSON object, adds headers to
those requests (for example a gateway's credentials).

    pip install -r conformance/requirements.txt
    python scripts/check_jsonld.py [--spec api/openapi.yaml] [--url https://tms.example.org]
"""

import argparse
import json
import os
import pathlib
import re
import sys
import urllib.request

import yaml
from pyld import jsonld

ROOT = pathlib.Path(__file__).resolve().parent.parent
RDF_TYPE = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
LIST = "/api/v1/institutions"
CONTEXT_REL = "http://www.w3.org/ns/json-ld#context"
EXTRA_HEADERS = json.loads(os.environ.get("API_EXTRA_HEADERS") or "{}")


def context_link(header):
    """The URL of the JSON-LD context link in a Link header, if any,
    whatever other links or parameter order it has."""
    for link in re.split(r",\s*(?=<)", header or ""):
        target = re.match(r"\s*<([^>]*)>(.*)", link)
        if target and re.search(r';\s*rel="?' + re.escape(CONTEXT_REL) + r'"?(\s*;|\s*$)', target[2]):
            return target[1]
    return None


class Mapping:
    """What the spec and a context say the list's fields map to."""

    def __init__(self, spec, context):
        schemas = spec["components"]["schemas"]
        ctx = context["@context"]
        prefixes = {k: v for k, v in ctx.items() if isinstance(v, str) and v.endswith(("/", "#"))}

        def expand(term):
            prefix, _, local = term.partition(":")
            return prefixes[prefix] + local if prefix in prefixes else term

        def definition(key):
            value = ctx.get(key)
            return value if isinstance(value, dict) else {"@id": value}

        def mapped(schema):
            for field, prop in schemas[schema]["properties"].items():
                term = prop.get("x-schema-org") or schemas.get(prop.get("$ref", "").rsplit("/", 1)[-1], {}).get("x-schema-org")
                if term and term != "@type":
                    yield field, expand(term), definition(field).get("@type")

        self.list_type = expand(schemas["InstitutionsResponse"]["x-schema-org"]["@type"])
        self.entry_type = expand(schemas["Institution"]["x-schema-org"]["@type"])
        # The list's own predicates (result) and each entry's.
        self.list_fields = list(mapped("InstitutionsResponse"))
        self.entry_fields = list(mapped("Institution"))
        self.predicates = {RDF_TYPE} | {p for _, p, _ in self.list_fields + self.entry_fields}
        # Vocabulary terms (provider values) the context maps.
        self.vocab = {k: expand(v) for k, v in ctx.items() if isinstance(v, str) and ":" in v and k not in prefixes}


def check(name, response, context, mapping):
    """The ways `response` read with `context` falls short, as messages."""
    failures = []
    triples = jsonld.to_rdf({**context, **response})["@default"]
    entries = response["result"]

    def objects(predicate):
        return [t["object"] for t in triples if t["predicate"]["value"] == predicate]

    def expect(ok, what):
        if not ok:
            failures.append(f"{name}: {what}")

    types = [o["value"] for o in objects(RDF_TYPE)]
    expect(types.count(mapping.list_type) == 1, f"one {mapping.list_type}")
    expect(types.count(mapping.entry_type) == len(entries), f"every entry a {mapping.entry_type}")
    orgs = {t["subject"]["value"] for t in triples if t["object"]["value"] == mapping.entry_type}
    for field, predicate, _ in mapping.list_fields:
        expect({o["value"] for o in objects(predicate)} == orgs, f"every entry linked directly by {field} ({predicate})")

    # Each mapped field counted against the entries, so an unmapped value
    # cannot pass by producing no triples.
    for field, predicate, kind in mapping.entry_fields:
        values = [e[field] for e in entries if e.get(field) is not None]
        if kind == "@vocab":
            expected = [mapping.vocab.get(v, f"unmapped {v}") for v in values]
        elif isinstance(values[0] if values else None, bool):
            expected = [json.dumps(v) for v in values]
        else:
            expected = [str(v) for v in values]
        found = objects(predicate)
        expect(sorted(o["value"] for o in found) == sorted(expected), f"every {field} as {predicate}")
        if kind in ("@vocab", "@id"):
            expect(all(o["type"] == "IRI" for o in found), f"{field} values as IRIs")

    leaked = {t["predicate"]["value"] for t in triples} - mapping.predicates
    expect(not leaked, f"only the mapped fields in the graph (envelope fields left out), not {sorted(leaked)}")
    return failures


def revision_of(url, spec):
    """The context revision a context URL names, by the spec's path template."""
    template = next(p for p in spec["paths"] if "{revision}" in p)
    pattern = re.escape(template).replace(re.escape("{revision}"), r"(\d+)") + "$"
    match = re.search(pattern, url or "")
    return match[1] if match else None


def from_spec(spec_path):
    spec = yaml.safe_load(spec_path.read_text())
    list_200 = spec["paths"][LIST]["get"]["responses"]["200"]
    # The context revision the spec's Link header example names.
    revision = revision_of(context_link(spec["components"]["headers"]["ContextLink"]["example"]), spec)
    if not revision:
        sys.exit("JSON-LD check failed: the spec's Link example names no context revision")
    context = json.loads((spec_path.parent / f"institutions.context.r{revision}.jsonld").read_text())
    mapping = Mapping(spec, context)
    examples = list_200["content"]["application/json"]["examples"]
    failures = [f for name, example in examples.items() for f in check(name, example["value"], context, mapping)]
    return failures, f"{len(examples)} example(s) with context r{revision}"


def fetch(url):
    return urllib.request.urlopen(urllib.request.Request(url, headers=EXTRA_HEADERS))


def from_url(spec_path, base):
    spec = yaml.safe_load(spec_path.read_text())
    with fetch(f"{base}{LIST}") as resp:
        link = context_link(resp.headers.get("Link"))
        response = json.load(resp)
    revision = revision_of(link, spec)
    if not revision:
        sys.exit("JSON-LD check failed: the list's Link header names no context revision")
    with fetch(link) as resp:
        context = json.load(resp)
    return check(base, response, context, Mapping(spec, context)), f"{base} with context r{revision}"


parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
parser.add_argument("--spec", type=pathlib.Path, default=ROOT / "api/openapi.yaml", help="the spec, with its context files next to it")
parser.add_argument("--url", help="base URL of a deployment to check, with no trailing slash")
args = parser.parse_args()
failures, checked = from_url(args.spec, args.url) if args.url else from_spec(args.spec)
if failures:
    sys.exit("JSON-LD check failed:\n  " + "\n  ".join(failures))
print(f"JSON-LD check passed for {checked}.")
