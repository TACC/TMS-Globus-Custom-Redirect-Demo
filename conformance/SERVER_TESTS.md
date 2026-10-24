# Tests an implementation must run itself

The black-box suite (`test_institutions_api.py`) cannot reach these: they
need control of the institution data, so they belong in the implementing
server's own tests, run against a test database. Each names the demo test
that covers it today (in `src/institutions_api.rs` or `src/institutions.rs`),
to port rather than copy: the demo reads a JSON file, a server reads its
database.

| # | Requirement (api/openapi.yaml) | Set up | Expect | Demo test |
|---|---|---|---|---|
| 1 | Data that cannot be read is a `503` | Make the data source unreachable (stop the database, revoke the role) | `503` `HttpResult` (`result_code: "503"`), `Cache-Control: no-store`, `nosniff`, also when `If-None-Match: *` is sent; `result_msg` names no host, table or file | `unreadable_registry_is_an_error` |
| 2 | Invalid data is a `500`, and none of it is served | Store one entry that breaks a schema rule (if the database's constraints allow it) | `500` `HttpResult`, as in 1; no partial list | `unreadable_registry_is_an_error` |
| 3 | Only approved entries are listed | Store approved and unapproved entries | Only the approved ones in `institutions`, and `num_institutions` their count | `list_holds_only_approved_entries_in_a_stable_order` |
| 4 | The order is stable while `data_version` is | Request twice | The same `institutions`, in the same order | `list_holds_only_approved_entries_in_a_stable_order` |
| 5 | Any change to `institutions` gives a new `data_version` | Insert, update, and delete an entry; approve and unapprove one | A new `data_version` after each, including the delete | — (the demo's file holds it by hand) |
| 6 | A new `data_version` gives a new `ETag` | Change the data | The old tag gets `200`, the new one `304` | `etag_follows_the_data` |
| 7 | Entries that break the schema are never served | Each rule in `Institution` (id pattern, unique ids, blank name, domain pattern, `https` sign-out URL, no domain for `tacc_tapis`) | Refused when written, or a `500` when read | `institutions::tests` |
| 8 | Only the API's fields are sent | Store an entry with every column set | Each entry has exactly the `Institution` fields (plus `type` with the addendum); no stored column such as `approved` or a client secret | `list_conforms_to_the_spec_and_addendum` |
| 9 | The login options without an institution are listed | Store, approved, a Globus entry with no `redirect_idp_domain` and a `tacc_tapis` entry | Both in `institutions`, with `redirect_idp_domain: null` | `list_holds_only_approved_entries_in_a_stable_order` (`globus`, `tacc_tapis`) |

Requirement 5 is the one most likely to fail with a database: a
`data_version` taken from the newest `updated` timestamp misses a deleted
row. Derive it from the served content instead (a hash of `institutions`), or
bump a revision counter in the same transaction as every write.

Run the black-box suite against the same server in CI as well, as
`run_against_demo.sh` does for the demo, or call
`.github/workflows/conformance-external.yml` after a deploy.
