#!/bin/bash
set -euo pipefail

# Script to build all historical versions of Ferrous with appropriate feature sets

echo "🔧 Building all Ferrous versions with version-specific features..."

# Ensure we're in the project root
cd "$(dirname "$0")"

# Create artifacts directories
mkdir -p artifacts/{v1.0.0,v2.0.0,v3.0.0,v4.0.0}

# Version to feature mapping
declare -A VERSION_FEATURES
VERSION_FEATURES[v1.0.0]="v1"
VERSION_FEATURES[v2.0.0]="v2"  
VERSION_FEATURES[v3.0.0]="v3"

# Build each version
for version in v1.0.0 v2.0.0 v3.0.0; do
    features="${VERSION_FEATURES[$version]}"
    echo "📦 Building $version with features: $features"
    
    # Update Cargo.toml version temporarily for build
    original_version=$(grep '^version = ' Cargo.toml | head -1)
    sed -i.bak "s/^version = .*/version = \"${version#v}\"/" Cargo.toml
    
    # Build with version-specific features
    echo "   🛠️  Compiling binaries..."
    cargo build --release --target x86_64-unknown-linux-musl \
        --features "$features" --no-default-features \
        --quiet
    
    # Copy binaries to version-specific directory
    cp target/x86_64-unknown-linux-musl/release/monitor-agent "artifacts/$version/"
    cp target/x86_64-unknown-linux-musl/release/monitor-collector "artifacts/$version/"
    
    # Get binary sizes
    agent_size=$(ls -lh "artifacts/$version/monitor-agent" | awk '{print $5}')
    collector_size=$(ls -lh "artifacts/$version/monitor-collector" | awk '{print $5}')
    
    echo "   ✅ Built $version: agent=$agent_size, collector=$collector_size"
    
    # Restore original version
    mv Cargo.toml.bak Cargo.toml
done

echo ""
echo "🎉 All versions built successfully!"
echo ""
echo "📁 Available binaries:"
for version in v1.0.0 v2.0.0 v3.0.0; do
    echo "   $version:"
    ls -lh "artifacts/$version/" | grep -E "(monitor-agent|monitor-collector)" | awk '{printf "     %s (%s)\n", $9, $5}'
done

echo ""
echo "🚀 Test version differences:"
echo "   ./artifacts/v1.0.0/monitor-agent --version"
echo "   ./artifacts/v2.0.0/monitor-agent --version"  
echo "   ./artifacts/v3.0.0/monitor-agent --version"
echo ""
echo "📋 Deploy specific versions:"
echo "   ./deploy.py build --version v1.0.0"
echo "   ./deploy.py all --version v2.0.0"
echo "   ./deploy.py versions"