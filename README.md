# Padel automatic court bookings

Polls Playtomic and books a padel court at your club when a slot matching your
targets opens up **and your (member) price is at or below `max_price`** (0 = free only).

Uses Playtomic's undocumented web API, so it can break without notice, and
automated booking is likely against Playtomic's terms. Use at your own risk.

## Setup

```sh
cargo build --release
export PLAYTOMIC_EMAIL=you@example.com PLAYTOMIC_PASSWORD=...
./target/release/padel-autobook search "club name"   # -> tenant_id
cp config.example.toml config.toml                  # set tenant_id + targets
./target/release/padel-autobook courts               # optional: court allowlist
./target/release/padel-autobook run --once           # dry run (default)
```

When the dry-run logs show the right slots, set `dry_run = false` and run
`padel-autobook run` (polls every `poll_interval_minutes`), or call `run --once` from cron.
Use `RUST_LOG=padel_autobook=debug` for more detail.

## How it decides

0. If `max_open_bookings` (default 3) booked slots are still ahead, stop.
1. For each day in `days_ahead` with a matching target and no booking yet, fetch availability.
2. Keep slots that match a target's weekday, start window and duration (and `courts`, if set).
3. For the best candidates, create a payment intent. This is Playtomic's checkout
   quote and returns *your* price, including member pricing. Paid slots are remembered
   for 7 days so they are not quoted again.
4. If the price is <= `max_price`, confirm it. Then that day is done.

`state.json` records bookings and `open_bookings`, the number not yet played.
Only bookings made by this tool are counted. If a confirmation fails or looks odd, the day is
marked `Uncertain` and blocked; check the app and remove the entry to retry.

The process exits instead of retrying when the limit is reached (also right after
a booking fills it), or when any request fails: a refused quote (e.g. a 409 conflict or the
club's own reservation limit), a rate limit, an unreadable price or a failed confirmation.
Fix the cause (or wait until a booking has been played) and start it again.
