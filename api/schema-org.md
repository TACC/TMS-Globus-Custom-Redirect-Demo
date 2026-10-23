# schema.org mapping of the institutions list

Background for the linked-data part of `openapi.addendum.yaml` and the
JSON-LD contexts (`institutions.context.r<n>.jsonld`). None of it is needed
to produce the list; it explains why the fields map to the terms they do.

The payload stays plain, flat JSON. The latest context is the mapping: it
gives every field of the spec's schemas a term, or `null` to leave it out
of the graph, and maps no field the spec lacks (`scripts/check_spec.py`
checks this).

- The response is a [schema:ItemList](https://schema.org/ItemList) and each
  entry a [schema:Organization](https://schema.org/Organization). The `type`
  fields carry those types into the graph; a context cannot add them on its
  own.
- The list links each entry directly with `schema:itemListElement`. The
  order of `result` is part of the JSON only; the graph does not record it.
- Fields with no schema.org term (IdP domain, IdP sign-out URL, login
  provider) use the `tms:` vocabulary (`https://tacc.utexas.edu/ns/tms#`).
  That namespace is not published yet, so its IRIs do not resolve.
- The envelope fields `status`, `message` and `data_version` describe the HTTP
  response, not the institutions, so the context leaves them out of the
  graph.
- `provider` values are vocabulary terms (`"@type": "@vocab"`): the known
  values `globus` and `tacc_tapis` expand to `tms:GlobusProvider` and
  `tms:TaccTapisProvider`. `provider` is an open set, so a new value needs a
  new context revision that maps it; until a client has that revision, an
  unmapped value expands to an IRI relative to the document's base.

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

schema.org, pending included, has no term for an IdP domain or a sign-out
URL, so those stay in `tms:`.
