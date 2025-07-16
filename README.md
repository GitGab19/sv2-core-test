# SV2 Core Test Repository

This is a test repository for the SV2 core protocols architecture.

## Contents

- `protocols/` - SV2 protocol implementations
- `common/` - Shared utilities and common code
- `utils/` - Utility crates (buffer, error-handling, key-utils, etc.)

## Local Integration Testing

To run integration tests locally:

```bash
./scripts/run-integration-tests.sh
```

This will:
1. Clone/update the integration test framework
2. Update dependencies to use your local changes
3. Run the full integration test suite
4. Restore the original configuration

## CI/CD

This repository automatically runs integration tests on every PR using the reusable workflow from the `sv2-integration-test-framework` repository.
