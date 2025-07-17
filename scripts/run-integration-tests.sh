#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
INTEGRATION_DIR="$REPO_ROOT/integration-test-framework/sv2-integration-test-framework-test"

echo "🧪 Running integration tests for sv2-miner-apps-test changes..."
echo "📁 Repository root: $REPO_ROOT"
echo "📁 Integration test dir: $INTEGRATION_DIR"

mkdir -p "$REPO_ROOT/integration-test-framework"

# Clone/update integration test framework
if [ ! -d "$INTEGRATION_DIR" ]; then
    echo "📥 Cloning integration test framework..."
    cd "$(dirname "$INTEGRATION_DIR")"
    git clone https://github.com/GitGab19/sv2-integration-test-framework-test.git
else
    echo "🔄 Updating integration test framework..."
    cd "$INTEGRATION_DIR"
    git fetch origin
    git reset --hard origin/main
fi

cd "$INTEGRATION_DIR"

# Update sv2-miner-apps-test dependencies to use local path
echo "🔧 Updating dependencies to use local sv2-core-test..."

# Use sed to replace git dependencies with local path dependencies in integration-tests/Cargo.toml
sed -i '' 's|stratum-common = { git = "https://github.com/GitGab19/sv2-core-test", branch = "main", features = \["with_network_helpers", "sv1"\] }|stratum-common = { path = "../../common", features = ["with_network_helpers", "sv1"] }|g' Cargo.toml
sed -i '' 's|sv1_api = { git = "https://github.com/GitGab19/sv2-core-test", branch = "main", optional = true }|sv1_api = { path = "../../sv1", optional = true }|g' Cargo.toml
sed -i '' 's|key-utils = { git = "https://github.com/GitGab19/sv2-core-test", branch = "main" }|key-utils = { path = "../../utils/key-utils" }|g' Cargo.toml
sed -i '' 's|config-helpers = { git = "https://github.com/GitGab19/sv2-core-test", branch = "main" }|config-helpers = { path = "../../roles-utils/config-helpers" }|g' Cargo.toml

# Add patch section to override all git dependencies with local paths
echo "🔧 Adding patch section to override git dependencies..."

# Remove any existing patch section first
sed -i '' '/^# Override git dependencies with local paths/,/^$/d' Cargo.toml
sed -i '' '/^\[patch\."https:\/\/github\.com\/GitGab19\/sv2-core-test"\]/,/^$/d' Cargo.toml

# Add the patch section at the end of the file
cat >> Cargo.toml << 'EOF'

# Override git dependencies with local paths to avoid version conflicts
[patch."https://github.com/GitGab19/sv2-core-test"]
stratum-common = { path = "../../common" }
sv1_api = { path = "../../sv1" }
key-utils = { path = "../../utils/key-utils" }
config-helpers = { path = "../../roles-utils/config-helpers" }
roles_logic_sv2 = { path = "../../sv2/roles-logic-sv2" }
network_helpers_sv2 = { path = "../../roles-utils/network-helpers" }
binary_sv2 = { path = "../../sv2/binary-sv2" }
binary_codec_sv2 = { path = "../../sv2/binary-sv2/codec" }
derive_codec_sv2 = { path = "../../sv2/binary-sv2/derive_codec" }
noise_sv2 = { path = "../../sv2/noise-sv2" }
framing_sv2 = { path = "../../sv2/framing-sv2" }
codec_sv2 = { path = "../../sv2/codec-sv2" }
common_messages_sv2 = { path = "../../sv2/subprotocols/common-messages" }
template_distribution_sv2 = { path = "../../sv2/subprotocols/template-distribution" }
mining_sv2 = { path = "../../sv2/subprotocols/mining" }
job_declaration_sv2 = { path = "../../sv2/subprotocols/job-declaration" }
channels_sv2 = { path = "../../sv2/channels-sv2" }
parsers_sv2 = { path = "../../sv2/parsers-sv2" }
buffer_sv2 = { path = "../../utils/buffer" }
error_handling = { path = "../../utils/error-handling" }
rpc_sv2 = { path = "../../roles-utils/rpc" }
EOF

echo "✅ Updated Cargo.toml to use local dependencies"
echo "🏃 Running integration tests..."

# Run the integration tests
cargo test --features sv1 --verbose

cd "$REPO_ROOT"
echo "✅ Integration tests completed!"