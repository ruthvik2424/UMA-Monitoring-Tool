#!/usr/bin/env python3
"""
Compare features available in different UMA versions.
Shows what functionality is enabled/disabled in each version.
"""

import subprocess
import sys
from pathlib import Path

def get_binary_info(binary_path):
    """Get version and feature info from a binary."""
    try:
        # Get version
        result = subprocess.run([str(binary_path), '--version'], 
                              capture_output=True, text=True, timeout=5)
        version = result.stdout.strip() if result.returncode == 0 else "Unknown"
        
        # Check size
        size_mb = binary_path.stat().st_size / (1024 * 1024)
        
        return {
            'version': version,
            'size_mb': f"{size_mb:.1f}MB",
            'exists': True
        }
    except Exception as e:
        return {
            'version': f"Error: {e}",
            'size_mb': "N/A",
            'exists': False
        }

def test_features(binary_path, binary_type):
    """Test which features are available in a binary."""
    features = {}
    
    if not binary_path.exists():
        return {"binary_missing": True}
    
    try:
        if binary_type == "agent":
            # Test debug snapshot (v3+ feature)
            result = subprocess.run([str(binary_path), '--debug-snapshot'], 
                                  capture_output=True, text=True, timeout=2)
            features['debug_endpoint'] = "feature not available" not in result.stderr.lower()
            
        # More feature tests can be added here
        features['binary_missing'] = False
        
    except Exception as e:
        features['test_error'] = str(e)
    
    return features

def main():
    print("🔍 UMA Version Comparison")
    print("=" * 50)
    
    artifacts_dir = Path("artifacts")
    if not artifacts_dir.exists():
        print("❌ No artifacts directory found. Run ./build-all-versions.sh first.")
        sys.exit(1)
    
    versions = ["v1.0.0", "v2.0.0", "v3.0.0"]
    
    # Compare agent binaries
    print("\n🤖 Agent Binary Comparison:")
    print(f"{'Version':<10} {'Size':<8} {'Version String':<25} {'Debug Endpoint':<15}")
    print("-" * 70)
    
    for version in versions:
        agent_path = artifacts_dir / version / "monitor-agent"
        info = get_binary_info(agent_path)
        features = test_features(agent_path, "agent") if info['exists'] else {}
        
        debug_support = "✅ Yes" if features.get('debug_endpoint', False) else "❌ No"
        if 'binary_missing' in features:
            debug_support = "❓ N/A"
            
        print(f"{version:<10} {info['size_mb']:<8} {info['version']:<25} {debug_support:<15}")
    
    # Compare collector binaries  
    print("\n🏢 Collector Binary Comparison:")
    print(f"{'Version':<10} {'Size':<8} {'Version String':<25}")
    print("-" * 50)
    
    for version in versions:
        collector_path = artifacts_dir / version / "monitor-collector"
        info = get_binary_info(collector_path)
        print(f"{version:<10} {info['size_mb']:<8} {info['version']:<25}")
    
    # Feature matrix
    print("\n🎛️  Feature Matrix:")
    print(f"{'Feature':<25} {'v1.0.0':<8} {'v2.0.0':<8} {'v3.0.0':<8}")
    print("-" * 55)
    
    features_matrix = [
        ("Core 17 Modules", "✅", "✅", "✅"),
        ("mTLS Transport", "✅", "✅", "✅"), 
        ("Web GUI", "✅", "✅", "✅"),
        ("Desktop Notifications", "❌", "✅", "✅"),
        ("Slack Webhooks", "❌", "✅", "✅"),
        ("Google Chat Webhooks", "❌", "✅", "✅"),
        ("BMC Enhancements", "❌", "✅", "✅"),
        ("Maintenance Mode", "❌", "❌", "✅"),
        ("Debug HTTP Endpoint", "❌", "❌", "✅"),
        ("Metrics Export", "❌", "❌", "✅")
    ]
    
    for feature, v1, v2, v3 in features_matrix:
        print(f"{feature:<25} {v1:<8} {v2:<8} {v3:<8}")
    
    print("\n📦 Binary Locations:")
    for version in versions:
        version_dir = artifacts_dir / version
        if version_dir.exists():
            binaries = list(version_dir.glob("monitor-*"))
            if binaries:
                print(f"   {version}: {len(binaries)} binaries in artifacts/{version}/")
            else:
                print(f"   {version}: ❌ No binaries found")
        else:
            print(f"   {version}: ❌ Directory missing")
    
    print("\n🚀 Next Steps:")
    print("   • Deploy specific version: ./deploy.py all --version v2.0.0")
    print("   • List versions: ./deploy.py versions")
    print("   • Build all versions: ./build-all-versions.sh")

if __name__ == "__main__":
    main()