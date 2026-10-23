"""Checks that the spec's example response, read with the JSON-LD context,
yields the RDF the spec promises: a typed schema:ItemList of typed
schema:Organizations, with every field mapped and the envelope left out.

    pip install pyld==3.3.0 pyyaml==6.0.3
    python scripts/check_jsonld.py
"""

import json
import pathlib
import sys

import yaml
from pyld import jsonld

ROOT = pathlib.Path(__file__).resolve().parent.parent
RDF_TYPE = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
SCHEMA = "https://schema.org/"
TMS = "https://tacc.utexas.edu/ns/tms#"

spec = yaml.safe_load((ROOT / "api/openapi.yaml").read_text())
context = json.loads((ROOT / "api/institutions.context.v1.jsonld").read_text())
examples = spec["paths"]["/login/institutions"]["get"]["responses"]["200"]["content"]["application/json"]["examples"]

failures = []
for name, example in examples.items():
    response = example["value"]
    triples = jsonld.to_rdf({**context, **response})["@default"]

    def objects(predicate):
        return [t["object"] for t in triples if t["predicate"]["value"] == predicate]

    def check(ok, what):
        if not ok:
            failures.append(f"{name}: {what}")

    types = [o["value"] for o in objects(RDF_TYPE)]
    check(types.count(SCHEMA + "ItemList") == 1, "one schema:ItemList")
    check(types.count(SCHEMA + "Organization") == len(response["result"]), "every entry a schema:Organization")
    orgs = {t["subject"]["value"] for t in triples if t["object"]["value"] == SCHEMA + "Organization"}
    check({o["value"] for o in objects(SCHEMA + "itemListElement")} == orgs, "every entry linked directly by schema:itemListElement")

    ids = sorted(o["value"] for o in objects(SCHEMA + "identifier"))
    check(ids == sorted(i["id"] for i in response["result"]), "every id as schema:identifier")
    check(len(objects(SCHEMA + "name")) == len(response["result"]), "every name as schema:name")
    check(len(objects(TMS + "approved")) == len(response["result"]), "every approved as tms:approved")

    providers = {o["value"] for o in objects(TMS + "provider")}
    check(providers <= {TMS + "GlobusProvider", TMS + "TaccTapisProvider"}, "providers as tms: IRIs")
    check(all(o["type"] == "IRI" for o in objects(TMS + "idpLogoutUrl")), "sign-out URLs as IRIs")
    domains = [i["redirect_idp_domain"] for i in response["result"] if i["redirect_idp_domain"]]
    check(sorted(o["value"] for o in objects(TMS + "redirectIdpDomain")) == sorted(domains), "domains as tms:redirectIdpDomain")

    envelope = {response[k] for k in ("status", "message", "version") if k in response}
    check(not envelope & {t["object"]["value"] for t in triples}, "envelope fields left out of the graph")

if failures:
    sys.exit("JSON-LD check failed:\n  " + "\n  ".join(failures))
print(f"JSON-LD check passed for {len(examples)} example(s).")
