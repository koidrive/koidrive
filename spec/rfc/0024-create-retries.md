# RFC 0024: Create retries

Status: proposed. Applies to spec v0 (format_version 1). Adds one field to each of four requests.

A client that loses the answer to a create cannot tell whether the server made the record. If it sends the request again, the server makes a second record, or refuses because the first one exists. A person who names a shared folder "Photos" on a flaky network then sees "A shared folder named Photos already exists", and an empty Photos folder stays on every device. This RFC lets a client send a create again and get back what the first request made. A bare section number such as §2 refers to this RFC.

## 1. Messages

New fields only:

| Request | Field | Endpoint |
|---|---|---|
| `CreateSpaceRequest` | `request_id = 2` | `POST /v1/spaces` |
| `CreateShareLinkRequest` | `request_id = 5` | `POST /v1/spaces/{space_id}/links` (RFC 0005) |
| `PairCompleteRequest` | `request_id = 4` | `POST /v1/pair/complete` |
| `ClaimRequest` | `request_id = 6` | `POST /v1/claim` |

`request_id` is a string of at most 64 characters. The client chooses a new random value for each intended create and sends the same value with every resend of that create. A longer value is refused with `INVALID_ARGUMENT`. An empty value asks for no retry handling: the server treats the request as it did before this RFC.

## 2. Server

The server stores each `request_id` with the record its request made, and checks it before anything else.

- **Space.** The server keys `request_id` by owner. A request whose `request_id` already made a space returns that space with 201, even though its name now exists. The same `request_id` with another name is refused with `INVALID_ARGUMENT`, even while the first request is still making its space, and nothing is made for it. A resend while the first request is still making the space is refused with `RETRY_LATER`. A name that another request already uses is refused with `INVALID_ARGUMENT` and HTTP 409, as before.
- **Share link.** The server keys `request_id` by space. A request whose `request_id` already made a link returns that link, its URL included, with 201. The same `request_id` with another `node_id`, expiry or password is refused with `INVALID_ARGUMENT`. A request with `expires_at_ms` 0 and `no_expiry` false matches a link that expires 7 days after it was made.
- **Pairing.** Completing a pairing keeps the pairing code until it expires and records on it the `request_id` and the device it made. A request with that code and the same `request_id` returns that device with a new refresh credential and a new access token, and the server replaces the stored refresh credential. Any other use of a used code is refused as a wrong code.
- **Claim with a claim code.** The claim records the same on the claim code. A claim with that code and the same `request_id` returns the owner's device with a new refresh credential, as for pairing.

A device that has refreshed an access token once (`last_seen_ms` is set) or is revoked is never returned again: its owner already holds a working credential, so a later request with the code is refused as before. Returning a device again needs both the code and the `request_id`. Before the code is used, that is within its lifetime: 10 minutes for a pairing code and one hour for a claim code. Once the code is used, its lifetime becomes 25 hours from that use, which outlasts the longest wait a daily-limit refusal asks for (RFC 0028), so a client that stops at such a refusal can resend after the limit resets.

A claim with a recovery key never returns an earlier device for its `request_id`, though the length limit of §1 still applies. Setup already handles a lost answer there with a new key and `setup_attempt` (`ClaimRequest` field 5).

The Account DO adds the columns `spaces.request_id`, `share_links.request_id`, `pairing_codes.request_id`, `pairing_codes.paired_device_id`, `claim_codes.request_id` and `claim_codes.claimed_device_id` in one migration, with unique indexes on `(owner_id, request_id)` for spaces and `(space_id, request_id)` for share links.

## 3. Clients

`koidrive_net::request_id` makes a `request_id` from 16 random bytes. `koidrive_net::resend_lost` sends a create up to four times. It sends again after a lost answer, a 5xx status or `RETRY_LATER`, and returns any other refusal at once. `koi` uses both for every create of §1 except the recovery-key claim of `koi setup`: shared folders in `koi setup` and `koi space create`, `koi share create`, `koi join` and `koi server claim`. The macOS app reaches the same code through `koi`.

`koi setup` reports that a shared folder name is taken only when the server answers 409. Any other failure is reported as the error it is.

A server without this RFC ignores `request_id`, so a resend after a lost answer gets the refusal or the second record it got before.
