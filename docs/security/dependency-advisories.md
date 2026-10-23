# Dependency advisory policy

Every release and relevant CI run executes `scripts/check-advisories.sh`. A
dedicated workflow also runs it every Monday so a newly published advisory is
detected without waiting for a repository change. The script audits the
complete lockfile and denies vulnerabilities, warnings, unmaintained packages,
unsound packages, and yanked releases. The SQLx 0.9.0 lockfile no longer
contains `rsa`, and the current audit runs without advisory exceptions.

The script retains its reachability guard for historical SQLx lockfiles: any
reachable `rsa` dependency fails the check. New exceptions require a written
reachability and impact analysis.
