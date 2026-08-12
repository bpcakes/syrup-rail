# Dependency advisory policy

Every release and relevant CI run executes `scripts/check-advisories.sh`. A
dedicated workflow also runs it every Monday so a newly published advisory is
detected without waiting for a repository change. The script audits the
complete lockfile and denies vulnerabilities, warnings, unmaintained packages,
unsound packages, and yanked releases except for the one documented case below.

## RUSTSEC-2023-0071 (`rsa` 0.9.10)

The advisory describes a timing side channel in the RSA crate and has no fixed
release. Syrup Rail does not compile or execute this implementation. Cargo locks
it as an optional dependency of SQLx's MySQL support, while the workspace
disables SQLx default features and enables only PostgreSQL.

The advisory script pairs the narrow audit exception with a complete
dependency-tree check. It fails before auditing if any locked version of `rsa`
becomes reachable from any workspace target or feature combination. If `rsa`
leaves the lockfile, the script stops passing the exception to `cargo audit`.
Remove this policy section when SQLx no longer locks the package or when a fixed
dependency is available. Do not add a second exception without a new written
reachability and impact analysis.
