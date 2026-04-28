# Changelog

## v0.5.0 - 2026-04-28

### Added

- Bundle the raw IMC round 5 day CSVs under `datasets/round5/` with the official filenames from the latest round 5 download.

### Changed

- Enforce the official round 5 position limit of `10` for all fifty round 5 products in the Rust engine.
- Update the bundled Python traders so round 5 products also use the official limit of `10`.
- Expose `--raw-csv-market-trades` so raw CSV replays can either keep only residual public market trades or pass through the full raw public tape.
- Tighten the crate packaging allowlist to publish only the official bundled dataset files instead of any stray local files under round dataset folders.
- Make round 5 the default zero-argument dataset now that it is the latest populated bundled round.
- Update the README and crate metadata for the round 5 dataset bundle.

## v0.4.9 - 2026-04-26

### Changed

- Whitelist the crate package contents so crates.io publishes ignore unrelated local dataset folders that are not part of the release.

## v0.4.8 - 2026-04-26

### Added

- Bundle the raw IMC round 4 day CSVs under `datasets/round4/` with the official filenames from the latest round 4 download.

### Changed

- Load `observations_*.csv` automatically whenever a paired IMC `prices_*.csv` dataset includes conversion-observation data.
- Make round 4 the default zero-argument dataset now that it is the latest populated bundled round.
- Update the README to document the bundled round 4 data and observation-file pairing behavior.

## v0.4.7 - 2026-04-24

### Changed

- Keep the full CLI product table in competition order, including round 3 products as `HYDROGEL_PACK`, `VELVETFRUIT_EXTRACT`, then `VEV_*` vouchers by increasing strike.

## v0.4.6 - 2026-04-24

### Added

- Add `traders/all_products_trader.py` as a diagnostic trader that attempts to trade every product with visible book liquidity.

### Changed

- Show every product in the default CLI product table instead of rolling lower-ranked products into `OTHER(+N)`.

## v0.4.5 - 2026-04-24

### Fixed

- Enforce the official round 3 position limits for `HYDROGEL_PACK`, `VELVETFRUIT_EXTRACT`, and all ten `VEV_*` vouchers.
- Add the same round 3 limits to the bundled default trader.

## v0.4.4 - 2026-04-24

### Added

- Bundle the raw IMC round 3 day CSVs under `datasets/round3/` with the official filenames from the latest round 3 download.

### Changed

- Make round 3 the default zero-argument dataset now that it is the latest populated bundled round.

## v0.4.3 - 2026-04-19

### Fixed

- Encode multi-run day suffixes without collisions so negative and positive day datasets no longer overwrite each other in persisted run directories.
- Keep human-readable run directory names for round/day splits using `day-1`, `day-0`, and `day+1` formatting.

## v0.4.2 - 2026-04-19

### Changed

- Clarify the README install instructions so end users can prefer the published crates.io binary via `cargo install rust_backtester --locked`.
- Document how to run the installed binary, how to update it, and how it differs from local development flows like `cargo run` and `cargo run --release`.

## v0.4.1 - 2026-04-18

### Added

- Bundle the raw IMC round 2 day CSVs under `datasets/round2/` with the official filenames from the latest round 2 download.

### Changed

- Make round 2 the default zero-argument dataset now that it is the latest populated bundled round.
- Update the README to document the bundled round 2 data and default dataset behavior.

## v0.4.0 - 2026-04-17

### Changed

- Replay raw `prices_*.csv` and `trades_*.csv` datasets with a dedicated CSV-tape matcher that crosses submission orders against the visible book first, keeps submission remainders resting for the current tick, and only carries residual bot-vs-bot trades into the public tape.
- Preserve price-time priority between visible book liquidity, resting submission orders, and replayed CSV bot trades so submission fills only occur when they should under the replay model.
- Mark bundle timelines, submission logs, and PnL using a stable mark price: current two-sided midpoint when available, last stable two-sided midpoint through one-sided or empty ticks, and a visible-side bootstrap only when no stable midpoint exists yet.

### Fixed

- Fail fast when a raw `prices_*.csv` file is missing its paired `trades_*.csv` instead of silently running with an empty market-trade tape.
- Add regression coverage for raw CSV replay ordering, missing trade-pair detection, and stable mark-price carry-forward behavior.

## v0.3.0 - 2026-04-16

### Changed

- Improve multi-run CLI output with a `TOTAL` row in the top summary table and a rightmost `TOTAL` column in the product table.
- Standardize day labels in CLI summaries to use `D-2`, `D-1`, `D=0`, and `D+N` formatting.
- Add regression tests for the new CLI summary label and total-column behavior.

## v0.2.2 - 2026-04-14

### Added

- Bundle the raw IMC round 1 day CSVs under `datasets/round1/` with the official filenames from the latest round 1 download.
- Make round 1 the default zero-argument dataset once it is the latest populated bundled round.

### Changed

- Add `INTARIAN_PEPPER_ROOT` and `ASH_COATED_OSMIUM` to the default trader and position-limit map with round 1 limits of `80` each.
- Update the README to document the bundled round 1 data and default dataset behavior.

## v0.2.1 - 2026-04-12

### Fixed

- Remove market trades from persisted bundle timelines once they have been consumed by the submission, preventing duplicate replay data in generated artifacts.
- Add regression coverage for consumed market trades so the bundled timeline and submission log stay in sync.
- Serialize Python trader module loading so embedded `datamodel` imports do not fail under parallel test execution.
