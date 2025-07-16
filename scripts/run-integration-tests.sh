#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
INTEGRATION_DIR="$REPO_ROOT/../sv2-integration-test-framework"

echo "🧪 Running integration tests for sv2-core-test changes..."
echo "📁 Repository root: $REPO_ROOT"
echo "📁 Integration test dir: $INTEGRATION_DIR"

# Clone/update integration test framework
if [ ! -d "$INTEGRATION_DIR" ]; then
    echo "📥 Cloning integration test framework..."
    cd "$(dirname "$INTEGRATION_DIR")"
    git clone https://github.com/GitGab19/sv2-integration-test-framework.git
else
    echo "🔄 Updating integration test framework..."
    cd "$INTEGRATION_DIR"
    git fetch origin
    git reset --hard origin/main
fi

cd "$INTEGRATION_DIR"

# Backup original Cargo.toml
cp Cargo.toml Cargo.toml.backup

# Update sv2-core-test dependency to use local path
echo "🔧 Updating dependencies to use local sv2-core-test..."
sed -i.bak "s|sv2-core-test = { git = .* }|sv2-core-test = { path = \"$REPO_ROOT\" }|" Cargo.toml

# Show what changed
echo "📝 Updated dependencies:"
grep "sv2-core-test" Cargo.toml || echo "No sv2-core-test dependency found"

# Run the tests
echo "🚀 Running integration tests..."
if [ $# -eq 0 ]; then
    cargo test --verbose
else
    cargo test --verbose "$@"
fi

# Restore original Cargo.toml
echo "🔄 Restoring original Cargo.toml..."
mv Cargo.toml.backup Cargo.toml

echo "✅ Integration tests completed!"
