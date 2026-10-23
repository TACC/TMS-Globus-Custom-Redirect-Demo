"""Checks that a list response, read with the JSON-LD context, yields the
RDF the spec promises: a typed schema:ItemList of typed schema:Organizations,
with every field mapped and the envelope left out.

By default it checks the spec's examples against the context file (next to
the spec) that its Link example names. With --url it checks a live
deployment instead: the list at <base>/api/v1/institutions and the context
its Link header points to.

    pip install -r conformance/requirements.txt
    python scripts/check_jsonld.py [--spec api/openapi.yaml] [--url https://tms.example.org]
"""

import argparse
import json
import pathlib
import re
import sys
import urllib.request

import yaml
from pyld import jsonld

ROOT = pathlib.Path(__file__).resolve().parent.parent
RDF_TYPE = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
SCHEMA = "https://schema.org/"
TMS = "https://tacc.utexas.edu/ns/tms#"
PROVIDERS = {"globus": TMS + "GlobusProvider", "tacc_tapis": TMS + "TaccTapisProvider"}
# Every predicate the graph may hold; anything else means an envelope field
# (or an unmapped one) leaked in.
PREDICATES = {RDF_TYPE} | {SCHEMA + p for p in ("itemListElement", "identifier", "name")} | {
    TMS + p for p in ("provider", "redirectIdpDomain", "idpLogoutUrl", "approved")
}
LINK = re.compile(r'<([^>]*/api/v1/institutions\.context\.r(\d+)\.jsonld)>; rel="http://www\.w3\.org/ns/json-ld#context"')


def check(name, response, context):
    """The ways `response` read with `context` falls short, as messages."""
    failures = []
    triples = jsonld.to_rdf({**context, **response})["@default"]

    def objects(predicate):
        return [t["object"] for t in triples if t["predicate"]["value"] == predicate]

    def expect(ok, what):
        if not ok:
            failures.append(f"{name}: {what}")

    types = [o["value"] for o in objects(RDF_TYPE)]
    expect(types.count(SCHEMA + "ItemList") == 1, "one schema:ItemList")
    expect(types.count(SCHEMA + "Organization") == len(response["result"]), "every entry a schema:Organization")
    orgs = {t["subject"]["value"] for t in triples if t["object"]["value"] == SCHEMA + "Organization"}
    expect({o["value"] for o in objects(SCHEMA + "itemListElement")} == orgs, "every entry linked directly by schema:itemListElement")

    ids = sorted(o["value"] for o in objects(SCHEMA + "identifier"))
    expect(ids == sorted(i["id"] for i in response["result"]), "every id as schema:identifier")
    expect(len(objects(SCHEMA + "name")) == len(response["result"]), "every name as schema:name")
    expect(len(objects(TMS + "approved")) == len(response["result"]), "every approved as tms:approved")

    # Counted against the entries, so an unmapped field cannot pass by
    # producing no triples.
    providers = objects(TMS + "provider")
    expected = sorted(PROVIDERS.get(i["provider"], f"unmapped {i['provider']}") for i in response["result"])
    expect(sorted(o["value"] for o in providers) == expected, "every provider as its tms: IRI")
    expect(all(o["type"] == "IRI" for o in providers), "providers as IRIs")
    urls = objects(TMS + "idpLogoutUrl")
    expected = sorted(i["idp_logout_url"] for i in response["result"] if i["idp_logout_url"])
    expect(sorted(o["value"] for o in urls) == expected, "every sign-out URL as tms:idpLogoutUrl")
    expect(all(o["type"] == "IRI" for o in urls), "sign-out URLs as IRIs")
    domains = sorted(i["redirect_idp_domain"] for i in response["result"] if i["redirect_idp_domain"])
    expect(sorted(o["value"] for o in objects(TMS + "redirectIdpDomain")) == domains, "domains as tms:redirectIdpDomain")

    leaked = {t["predicate"]["value"] for t in triples} - PREDICATES
    expect(not leaked, f"only the mapped fields in the graph (envelope fields left out), not {sorted(leaked)}")
    return failures


def from_spec(spec_path):
    spec = yaml.safe_load(spec_path.read_text())
    list_200 = spec["paths"]["/api/v1/institutions"]["get"]["responses"]["200"]
    # The context revision the spec's Link header example names.
    link = LINK.search(spec["components"]["headers"]["ContextLink"]["example"])
    if not link:
        sys.exit("JSON-LD check failed: the spec's Link example names no context revision")
    context = json.loads((spec_path.parent / f"institutions.context.r{link[2]}.jsonld").read_text())
    examples = list_200["content"]["application/json"]["examples"]
    failures = [f for name, example in examples.items() for f in check(name, example["value"], context)]
    return failures, f"{len(examples)} example(s) with context r{link[2]}"


def from_url(base):
    with urllib.request.urlopen(f"{base}/api/v1/institutions") as resp:
        link = LINK.search(resp.headers.get("Link", ""))
        response = json.load(resp)
    if not link:
        sys.exit("JSON-LD check failed: the list's Link header names no context revision")
    with urllib.request.urlopen(link[1]) as resp:
        context = json.load(resp)
    return check(base, response, context), f"{base} with context r{link[2]}"


parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
parser.add_argument("--spec", type=pathlib.Path, default=ROOT / "api/openapi.yaml", help="the spec, with its context files next to it")
parser.add_argument("--url", help="base URL of a deployment to check, with no trailing slash")
args = parser.parse_args()
failures, checked = from_url(args.url) if args.url else from_spec(args.spec)
if failures:
    sys.exit("JSON-LD check failed:\n  " + "\n  ".join(failures))
print(f"JSON-LD check passed for {checked}.")
