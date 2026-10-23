# idp_redirect_demo

A minimal Rust/axum example of using Globus Auth's `redirect_idp_domain`
authorize parameter, driven by a **mocked JSON list of approved institutions**,
plus a **test OpenID Connect provider** that plays the TACC login page.
It is shaped after `tms_portal`'s `application/src/routes/login.rs`
(`/login` → Globus authorize redirect → `/login/callback`), with the one
addition being `redirect_idp_domain`.

> `redirect_idp_domain` replaces the `idp_redirect_domain` parameter in the
> Globus Auth developer guide, which Globus does not act on (every domain we
> tried, including the guide's own `uchicago.edu`, still showed the Globus
> organization picker). Per Globus support, the new parameter ships to
> production on 2026-10-22.

## What it shows

1. `GET /api/v1/institutions` serves the approved entries of the registry
   file `mock/approved_institutions.json` (`{ version, institutions }`) in a
   Tapis-style envelope, `type / status / message / version / result`, as
   specified in `api/openapi.yaml`. The registry is checked against the
   spec's rules when it loads (unique ids, lower-case domains, `https`
   sign-out URLs, no domain for Tapis), so a bad entry stops the server
   instead of being served. Each request reads the file again, so edits
   take effect without a restart; if it has gone missing the list answers
   503, and if it no longer passes the checks, 500 (RFC 9457 problem
   details, as the spec gives them). The list is public, so
   `approved: false` entries are left out of it. They stay in the registry:
   `/login?institution=<id>` for one of them does not start a login but
   answers with a **Filtered** page (laid out like **Welcome**, with the
   institution's details) explaining that it is not on the approved list,
   and never redirects to Globus or Tapis.
2. `GET /login?institution=tacc` looks the id up in that list (unknown ids
   get a 403, unapproved ones the **Filtered** page), then redirects to:

   ```
   https://auth.globus.org/v2/oauth2/authorize
     ?response_type=code&client_id=…&redirect_uri=…/login/callback
     &scope=openid+profile+email&state=…
     &redirect_idp_domain=tacc.utexas.edu
   ```

   A user who isn't already signed in to Globus skips the Globus login page and
   goes straight to the authorization endpoint of the IdP Globus has registered
   for that domain.
3. `GET /login/callback` checks the state cookie, exchanges the code, calls
   Globus `userinfo`, and **verifies the returned `preferred_username` is
   `@<redirect_idp_domain>`**. This matters: `redirect_idp_domain` is a
   convenience redirect, not an access control. A user already signed in with
   another identity, or one who edits the URL, can come back as someone else,
   so the server has to check.

Add `&dry_run=true` to `/login` to get the authorize URL as JSON instead of
being redirected. For an unapproved institution it answers 403 with the
blocked message, `"blocked": true`, and the authorize URL it would have used
if approved (shown under the message in the "Preview URL" popup).

Two entries in the mock list are not institution redirects:

- **Globus (default login, no redirect)** (`id: globus`, `redirect_idp_domain: null`)
  sends the authorize request without `redirect_idp_domain`, so you see
  Globus's normal organization picker. Any identity is accepted on return.
- **TACC via Tapis (TaccTapis)** (`id: tacc_tapis`, `provider: tacc_tapis`)
  logs in directly against Tapis OAuth2 at `tacc.tapis.io`, not through
  Globus, like `tms_portal`'s `TaccTapis` provider: `/v3/oauth2/authorize`,
  then a form post to `/v3/oauth2/tokens` with HTTP Basic client credentials.
  The identity (`sub`, e.g. `jdoe@tacc`) is read from the access token's
  claims. It needs a Tapis OAuth2 client; set `TAPIS_CLIENT_ID` /
  `TAPIS_CLIENT_SECRET`. Without them the entry returns a 503 and the rest of
  the demo still works. Tapis only accepts the client's registered callback
  URL, so set `TAPIS_CALLBACK_URL` to it if it is not
  `<BASE_URL>/login/callback`: the demo then also serves that URL (on its own
  listener if the host or port differs) and relays the response to
  `/login/callback`.
  The local `dev.env` uses the TMS-prototyping dev client (values copied
  from `../../TMS-prototyping/dev.env`). That client is on the
  `portals.tapis.io` tenant with callback
  `http://127.0.0.1:8000/oauth-callback`, so port 8000 must be free.

After a login, the **Log in** button of the entry you used becomes a red
**Log out**. Clicking it opens that provider's sign-out page in a new tab
(Globus's `/v2/web/logout`, or Tapis's `/v3/oauth2/logout`, where you
confirm), since both otherwise remember you: Globus then skips the
`redirect_idp_domain` redirect, and Tapis skips its login form. It also
revokes the token, ends the demo's session (`POST /logout?local=true`), and
turns back into **Log in**. In mock mode no tab opens. The landing page's
**Log Out** works the same way and then reloads the landing page, since
Globus's sign-out page does not redirect back by itself.

For institution entries, ending the Globus session is not enough: the
institution's own IdP still has a session and signs you straight back in.
Globus does not pass `prompt=login` (or `max_age`) on to CILogon, and no
`ForceAuthn` reaches the IdP's SAML request, so the app cannot force a
password prompt from the authorize request. Instead each entry can name its
IdP's sign-out URL as `idp_logout_url` in `mock/approved_institutions.json`.
Globus's sign-out page then shows **Continue to <institution> sign-out**, which
goes to `/logout/idp?institution=<id>` and on to that URL (only URLs from
the approved list are used). Each was checked in a browser to complete a
sign-out:

| Entry | Login chain after CILogon | `idp_logout_url` |
|---|---|---|
| UT Austin | Shibboleth `enterprise.login.utexas.edu` | `…/idp/profile/Logout` |
| UChicago | Cirrus proxy `shibboleth2.uchicago.edu` → Okta | `uchicago.okta.com/login/signout` |
| Illinois | Shibboleth `shibboleth.illinois.edu` → Microsoft (tenant `44467e6f-…`) | `shibboleth.illinois.edu/idp/profile/Logout` |
| SDSC | UCSD Shibboleth `a5.ucsd.edu` | `a5.ucsd.edu/tritON/profile/Logout` |
| Stanford | Shibboleth `login.stanford.edu` | `…/idp/profile/Logout` |
| USC | Shibboleth `shibboleth.usc.edu` | `…/idp/profile/Logout` |
| MIT | Touchstone proxy `idp.mit.edu` → Okta | `okta.mit.edu/login/signout` |

Illinois's Shibboleth sign-out ends its own session and then hands off to
Microsoft's (`wsfederation?wa=wsignout1.0`), so one URL clears both. The
UChicago and MIT proxies' own sign-outs (`shibboleth2.uchicago.edu/idp/profile/Logout`,
`idp.mit.edu/idp/profile/Logout`) end only the proxy session, so those
entries sign out of Okta, which is what asks for the password; the proxy
may still sign you in silently while its own session lasts. SDSC has no IdP
of its own in Globus (`sdsc.edu` stops at the organization picker), so its
entry uses UC San Diego's `ucsd.edu`, and SDSC users come back as
`@ucsd.edu` identities. USC and MIT are in the registry but not approved:
their Globus details are real, but they are left out of the public list and
`/login?institution=usc` shows the **Filtered** page.

## The test IdP (`src/test_idp.rs`)

`redirect_idp_domain` only redirects to an IdP that is registered with Globus
Auth for that domain. Globus sends the user to that IdP's
`authorization_endpoint`, so "your custom login page" is simply the
authorization endpoint of an OIDC server. The demo serves one under `/idp`:

| Route                                    | Purpose                                         |
|------------------------------------------|-------------------------------------------------|
| `GET  /idp/.well-known/openid-configuration` | Discovery document (what Globus registers)  |
| `GET  /idp/jwks.json`                    | RS256 public key                                |
| `GET  /idp/authorize`                    | **The custom login page.** Shows the request it received and its `Referer`; logs a `[test-idp] authorize` line |
| `POST /idp/authorize`                    | Login form → redirect to `redirect_uri?code&state` |
| `POST /idp/token`                        | Code → `access_token` + signed `id_token` (`client_secret_basic` or `_post`) |
| `GET  /idp/userinfo`                     | Claims for a bearer access token                |

Users sign in with a bare username and come back as
`<username>@<IDP_DOMAIN>` (`sub`, `preferred_username` and `email`). Codes are
single-use, expire in two minutes, and are bound to the client and
`redirect_uri`. Only the registered client (`IDP_CLIENT_ID`) and
`redirect_uri`s are accepted.

`tms_portal` cannot be registered for this as it stands: its `/oauth2` routes
broker to an upstream IdP (Globus itself), its discovery and token responses
are wrapped in the TMS response envelope, its token endpoint takes JSON rather
than a form post, and it returns an empty `id_token`.

## Settings: `dev.env`

All settings are read from `dev.env` in the working directory: `KEY=VALUE`
lines, untracked (it holds secrets). Start from the template:

```bash
cp dev.env.example dev.env   # then fill in GLOBUS_CLIENT_ID / GLOBUS_CLIENT_SECRET, etc.
```

A real environment variable with the same name overrides the file (handy for
one-offs such as `GLOBUS_MOCK=true cargo run`), empty values use the
default, and `DEV_ENV_FILE=<path>` (environment only) reads another file.
See [Configuration](#configuration) for every setting.

## Run it against real Globus

With `GLOBUS_CLIENT_ID` and `GLOBUS_CLIENT_SECRET` in `dev.env`:

```bash
./run_real_globus.sh     # or: cargo run
# open http://localhost:8080
```

**Log in** sends you to `auth.globus.org` with the institution's
`redirect_idp_domain`, and on to that institution's real login page (e.g.
`utexas.edu` → UT EID, `uchicago.edu` → UChicago Okta). Your Globus app must
list `http://localhost:8080/login/callback` (or `<BASE_URL>/login/callback`)
as a redirect URI. Use a private window: an existing Globus session is not
redirected. The app refuses to start without both credentials unless
`GLOBUS_MOCK=true` is set. `run_real_globus.sh` builds a release binary and
forces real Globus Auth (`GLOBUS_MOCK=false`), whatever `dev.env` says.

## Run it offline (no Globus account needed)

```bash
GLOBUS_MOCK=true cargo run
# open http://localhost:8080
```

With `GLOBUS_MOCK=true`, the authorize redirect points at a built-in
`/mock-globus/authorize` page.

- **TACC** (`tacc.utexas.edu`, the test IdP's domain): mock Globus does what
  real Globus will do once the IdP is registered. It sends you straight to
  `/idp/authorize`, then redeems the code at `/idp/token` and verifies the
  `id_token` against the JWKS, exactly as a client would
  (`test_idp::relying_party_login`). Any failure there is shown as "Test IdP
  failed the client checks". This is a conformance check of the IdP before
  you register it.
- **Other institutions**: the mock page shows the `redirect_idp_domain` it
  received and lets you choose which identity "comes back". Use
  `jdoe@uchicago.edu` for the success path, or `jdoe@globusid.org` to see the
  callback reject a non-matching identity.

## Test `redirect_idp_domain` against real Globus

1. **Host the demo on public HTTPS.** Globus Auth calls the discovery, token
   and JWKS endpoints itself, so `localhost` will not do. Use a host whose
   domain you can prove, e.g. `https://idp-test.tacc.utexas.edu`, behind a
   TLS-terminating reverse proxy:

   In `dev.env`:

   ```bash
   BASE_URL=https://idp-test.tacc.utexas.edu
   BIND_ADDR=127.0.0.1:8080
   IDP_CLIENT_SECRET=<openssl rand -hex 32>   # keep it; Globus registers it
   GLOBUS_MOCK=true                           # real credentials come in step 3
   ```

   then `cargo run --release`.

   Check `curl $BASE_URL/idp/.well-known/openid-configuration` from outside.
   `idp_signing_key.pem` is created on first run; keep it, so the JWKS
   Globus has seen stays valid.

2. **Register the test IdP with Globus Auth.** On a Globus Connect Server v5
   data transfer node that has been through `node setup`, as root
   ([reference](https://docs.globus.org/globus-connect-server/v5/reference/oidc/register/)):

   ```bash
   globus-connect-server oidc register \
       --display-name "TACC (test)" \
       --discovery-url "https://idp-test.tacc.utexas.edu/idp/.well-known/openid-configuration" \
       --domain "tacc.utexas.edu" \
       --client-id "globus-auth-client" \
       --client-secret "$IDP_CLIENT_SECRET" \
       --username-claim "preferred_username" \
       --id-claim "sub" \
       --support-contact "<name>" \
       --support-email "<email>"
   ```

   Domain ownership is proven with a DNS TXT record on each domain containing
   the endpoint's client id; the registration fails until it exists, and can
   be rerun with the same arguments. Globus returns users to
   `https://auth.globus.org/p/authenticate/callback`, which is the default
   `IDP_REDIRECT_URIS`.

3. **Point the demo at real Globus.** Add
   `https://idp-test.tacc.utexas.edu/login/callback` as a redirect URI on
   your Globus app at <https://app.globus.org/settings/developers>, then
   set `GLOBUS_CLIENT_ID` / `GLOBUS_CLIENT_SECRET` in `dev.env`, clear
   `GLOBUS_MOCK`, and restart.

4. **Run the test** in a private window (an existing Globus session is not
   redirected): open `$BASE_URL`, click **Login**, then **Log in** for TACC.
   - Pass: you land directly on the orange "TACC test login" page (its
     `referer` row shows `auth.globus.org`, and the server logs
     `[test-idp] authorize`), sign in, and finish on "Welcome" with
     `jdoe@tacc.utexas.edu`.
   - Fail: the Globus organization picker appears. Use `Preview URL` to confirm
     `redirect_idp_domain=tacc.utexas.edu` was sent.
   - Token problems: Globus's calls show up as `[test-idp] token …` lines.

## Configuration

Set in `dev.env` (template: `dev.env.example`); environment variables override.

| Setting                 | Default                                            |
|-------------------------|----------------------------------------------------|
| `BASE_URL`              | `http://localhost:8080` (no trailing `/`)          |
| `BIND_ADDR`             | `127.0.0.1:8080`                                   |
| `INSTITUTIONS_FILE`     | `mock/approved_institutions.json`                  |
| `CORS_ALLOWED_ORIGINS`  | unset → same-origin only; `*` → any origin; or comma-separated origins (`https://host[:port]`) that may read `/api/v1/institutions` |
| `GLOBUS_CLIENT_ID`      | required unless `GLOBUS_MOCK=true`                 |
| `GLOBUS_CLIENT_SECRET`  | required unless `GLOBUS_MOCK=true`                 |
| `GLOBUS_MOCK`           | unset → real Globus Auth; `true` → built-in mock (also stands in for Tapis) |
| `TAPIS_CLIENT_ID`       | unset → TaccTapis entry disabled                   |
| `TAPIS_CLIENT_SECRET`   | unset → TaccTapis entry disabled                   |
| `TAPIS_BASE_URL`        | `https://tacc.tapis.io`                            |
| `TAPIS_CALLBACK_URL`    | `<BASE_URL>/login/callback`                        |
| `IDP_DOMAIN`            | `tacc.utexas.edu`                                  |
| `IDP_CLIENT_ID`         | `globus-auth-client`                               |
| `IDP_CLIENT_SECRET`     | random per run (printed at startup)                |
| `IDP_REDIRECT_URIS`     | `https://auth.globus.org/p/authenticate/callback` (comma-separated; the mock callback is always added) |
| `IDP_SIGNING_KEY`       | `idp_signing_key.pem` (created if missing)         |

The test IdP's issuer is always `<BASE_URL>/idp`.

## Mapping to TMS

- In `tms_portal`, the authorize URL is built in `login_handler` from the
  `identity_providers` row (`identity_redirect_url`, `client_id`, `scope`).
  Adding `("redirect_idp_domain", domain)` to its `query_params` vector is the
  same change shown in `src/globus.rs::build_authorize_url`.
- The approved list would naturally live in a DB table next to
  `identity_providers` / `allowed_redirects`, served at
  `/api/v1/institutions`. The mocked JSON stands in for that.
- The callback check corresponds to a guard in `handle_callback` before
  `make_auth_token`, using the `preferred_username` / `organization` claims
  that `jwt_utils.rs` already reads.
- The flow is the standard authorization-code grant described in the Tapis
  authentication docs: one controller redirects to the authorize endpoint
  with `client_id`, `redirect_uri` and `response_type=code`, and a second
  handles the callback and exchanges the code at the token endpoint.

## Tests

```bash
cargo test
npx @redocly/cli@2.60.0 lint                  # the spec, configured by redocly.yaml
pip install -r conformance/requirements.txt
python scripts/check_jsonld.py                # the spec's example
conformance/run_against_demo.sh               # conformance suite + live JSON-LD check, per CORS setting
```

CI (`.github/workflows/ci.yml`) runs all of them.

The API contract tests in `src/institutions_api.rs` call the API in-process
and validate the responses against the schemas in `api/openapi.yaml`, check
their headers (`Link`, `Cache-Control`, the weak `ETag`
`W/"<version>;r<n>"`, `If-None-Match` parsed by the RFC 9110 grammar and
`If-Match`, `304 Not Modified`, `HEAD`, CORS and the `OPTIONS` preflight on
the list for each `CORS_ALLOWED_ORIGINS` setting, CORS on the context), the
RFC 9457 errors (404, 405, 412, and 500/503 from a broken or missing
registry file), and check that every field the spec maps with
`x-schema-org` maps to the same term in the JSON-LD context.
`scripts/check_jsonld.py` expands the spec's example with that context and
checks the resulting RDF, counting each mapped field against the entries;
with `--url <base>` it does the same for a live deployment's list and the
context its `Link` header names, and `--spec <path>` checks another copy of
the spec.

`conformance/` is a black-box suite for any implementation of the spec, in
any language: it takes every expectation from the spec (including the
SHA-256 of each context revision, `x-context-sha256`), so it can be copied
next to any copy of `openapi.yaml`.

```bash
API_BASE_URL=https://tms.example.org API_CORS='https://portal.example.org' \
    python -m pytest conformance            # API_SPEC=<path> for another copy of the spec
```

`API_CORS` is the deployment's allowed-origin setting (empty, `*`, or
comma-separated origins). Header lists are compared as sets, and
`Cache-Control` as the directives it must carry, as the spec's "Header
values" section allows. `run_against_demo.sh` starts the demo three times,
once per kind of setting, and runs the suite and `check_jsonld.py --url`
against each. The 500 and 503 responses cannot be caused from outside, so
only the in-process tests cover them.

The context is revised separately from the API: each revision
(`api/institutions.context.r<n>.jsonld`, served at
`/api/v1/institutions.context.r<n>.jsonld`) is cached indefinitely, so it never
changes; a test fails if it does. Revision 2 is revision 1 without
`@version: 1.1`, which it did not need. To change the context, add the next
revision (`.r3.jsonld`) with rows for it in `CONTEXTS` and the tests'
`CONTEXT_SHA256` (in `src/institutions_api.rs`), add it to the spec's
`revision` enum and its hash to `x-context-sha256`, and point the spec's
`Link` header, `ETag` examples and context example at it. The list's `ETag`
names the context revision, so it changes with it. Every revision stays served, and the tests check that each is
unchanged, has its own file, is documented in the spec, and that the `Link`
header and the spec's examples name the latest. `api/schema-org.md` explains
the choice of linked-data terms.

The other tests cover: registry validation, only known institutions can be looked up, the authorize URL carries
`redirect_idp_domain`, identity/domain matching (including look-alike
domains such as `evil-uchicago.edu`), and for the test IdP: JWT signatures
verify only against its own JWKS, codes are single-use and bound to
client/`redirect_uri`, and a full discovery → token → `id_token` check over
real HTTP (including a rejected bad client secret).

## Not production-ready

The state cookie is unsigned and has no expiry, there is no session after
login, the test IdP has no real passwords and keeps codes and tokens in
memory, and the mock endpoints should never be deployed. TMS already handles
these with encoded state, DB-backed nonces and its own JWTs.
