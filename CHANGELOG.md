# Changelog

## v0.2.1 - 2026-04-12

### Fixed

- Remove market trades from persisted bundle timelines once they have been consumed by the submission, preventing duplicate replay data in generated artifacts.
- Add regression coverage for consumed market trades so the bundled timeline and submission log stay in sync.
- Serialize Python trader module loading so embedded `datamodel` imports do not fail under parallel test execution.
