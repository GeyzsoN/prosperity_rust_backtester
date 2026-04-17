# Changelog

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
