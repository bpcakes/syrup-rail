# Changelog

All notable changes to the Syrup Rail crates are documented in this file.

## [Unreleased]

## [0.1.1] - 2026-08-09

### Fixed

- Normalize PostgreSQL timestamps to database precision so round trips do not
  produce false optimistic-state mismatches.

### Changed

- Centralize locked payment-attempt terms and subscriber-readiness policy.
- Consolidate payment outcome, approved-application, and fallback processor
  evidence persistence while preserving the existing public constructors.

### Maintenance

- Restore clean-runner Jig bootstrap and enforce schema-backed SQLx metadata in
  repository policy CI.

## [0.1.0] - 2026-08-08

- Initial crates.io release of `syrup-rail`, `syrup-rail-postgres`,
  `syrup-rail-nmi`, and `syrup-rail-nmi-client`.

[Unreleased]: https://github.com/bpcakes/syrup-rail/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/bpcakes/syrup-rail/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/bpcakes/syrup-rail/tree/v0.1.0
