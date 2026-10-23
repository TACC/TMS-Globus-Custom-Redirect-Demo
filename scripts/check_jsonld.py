"""Checks that a list response, read with the JSON-LD context, yields the
RDF the spec promises: a typed schema:ItemList of typed schema:Organizations,
with every field mapped and the envelope left out.

By default it checks the spec's examples against the context file their Link
example names. With --url it checks a live deployment instead: the list at
<base>/api/v1/institutions and the context its Link header points to.

    pip install -r conformance/requirements.txt
    python scripts/check_jsonld.py [--url https://tms.example.org]
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

    providers = {o["value"] for o in objects(TMS + "provider")}
    expect(providers <= {TMS + "GlobusProvider", TMS + "TaccTapisProvider"}, "providers as tms: IRIs")
    expect(all(o["type"] == "IRI" for o in objects(TMS + "idpLogoutUrl")), "sign-out URLs as IRIs")
    domains = [i["redirect_idp_domain"] for i in response["result"] if i["redirect_idp_domain"]]
    expect(sorted(o["value"] for o in objects(TMS + "redirectIdpDomain")) == sorted(domains), "domains as tms:redirectIdpDomain")

    envelope = {response[k] for k in ("status", "message", "version") if k in response}
    expect(not envelope & {t["object"]["value"] for t in triples}, "envelope fields left out of the graph")
    return failures


def from_spec():
    spec = yaml.safe_load((ROOT / "api/openapi.yaml").read_text())
    list_200 = spec["paths"]["/api/v1/institutions"]["get"]["responses"]["200"]
    # The context revision the spec's Link header example names.
    link = LINK.search(list_200["headers"]["Link"]["example"])
    if not link:
        sys.exit("JSON-LD check failed: the spec's Link example names no context revision")
    context = json.loads((ROOT / f"api/institutions.context.r{link[2]}.jsonld").read_text())
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
parser.add_argument("--url", help="base URL of a deployment to check, with no trailing slash")
args = parser.parse_args()
failures, checked = from_url(args.url) if args.url else from_spec()
if failures:
    sys.exit("JSON-LD check failed:\n  " + "\n  ".join(failures))
print(f"JSON-LD check passed for {checked}.")
