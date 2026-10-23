# schema.org mapping of the institutions list

Background for `openapi.yaml` and the JSON-LD contexts
(`institutions.context.r<n>.jsonld`). None of it is needed to produce the
list; it explains why the fields map to the terms they do.

The payload stays plain, flat JSON. Each schema and field in the spec names
the linked-data term it means with `x-schema-org`, and the latest context
holds the same mapping (the `spec_and_context_agree` test checks this).

- The response is a [schema:ItemList](https://schema.org/ItemList) and each
  entry a [schema:Organization](https://schema.org/Organization). The `type`
  fields carry those types into the graph; a context cannot add them on its
  own.
- The list links each entry directly with `schema:itemListElement`. The
  order of `result` is part of the JSON only; the graph does not record it.
- Fields with no schema.org term (IdP domain, IdP sign-out URL, approval,
  login provider) use the `tms:` vocabulary (`https://tacc.utexas.edu/ns/tms#`).
  That namespace is not published yet, so its IRIs do not resolve.
- The envelope fields `status`, `message` and `version` describe the HTTP
  response, not the institutions, so the context leaves them out of the
  graph.
- `provider` values are vocabulary terms (`"@type": "@vocab"`): `globus`
  and `tacc_tapis` expand to `tms:GlobusProvider` and `tms:TaccTapisProvider`.
  A new provider value needs a new context revision that maps it.

## Related terms

These schema.org terms, including ones from
[pending.schema.org](https://pending.schema.org/docs/pending.home.html), are
the closest in meaning. They need nested objects (an action, a
certification) that a flat field cannot express, so they record intent
rather than map a field:

| Field | Related terms |
|---|---|
| `provider` | `schema:AuthenticateAction` (pending, issue 4507), `schema:provider` |
| `redirect_idp_domain` | `schema:AuthenticateAction`: signing in to the institution's IdP |
| `approved` | `schema:Certification` / `schema:certificationStatus` (core): a Certification with status `CertificationActive` or `CertificationInactive` |

schema.org, pending included, has no term for an IdP domain or a sign-out
URL, so those stay in `tms:`.
