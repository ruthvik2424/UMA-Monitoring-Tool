# UMA Version Management Guide

This document explains how version management works in the UMA project.

## Version Structure

UMA follows **Semantic Versioning** (semver.org):
- `vX.Y.Z` format (e.g., `v3.0.0`, `v4.1.2`)
- **Major** (X): Breaking changes, major features
- **Minor** (Y): New features, backward compatible  
- **Patch** (Z): Bug fixes, security updates

## Current Version History

### v1.0.0 - Base System
**Core monitoring with 17 modules**
- Basic agent + collector architecture
- mTLS WebSocket transport
- Web GUI with alerts and heatmap
- Ansible deployment system

### v2.0.0 - Notifications & Integrations  
**Added external integrations**
- Desktop notifications in browser
- Slack webhook integration
- Google Chat webhook integration
- BMC fixes and enhancements

### v3.0.0 - Maintenance & Metrics (Current)
**Operational features**  
- Maintenance mode (suppress alerts)
- Debug HTTP endpoint (metrics export)
- Enhanced configuration validation

### v4.0.0+ - Future Development
**Planned improvements** (TBD)
- Advanced analytics features
- Enhanced GUI capabilities
- Additional integrations
- Performance optimizations

## File Organization

### Source Code Versions
All versions share the same codebase with feature flags:
```
- Current codebase contains all v1, v2, v3 features
- Features are controlled by configuration
- Binary compatibility maintained across versions
```

### Binary Artifacts
Version-specific binaries stored in:
```
artifacts/
├── v1.0.0/
│   ├── monitor-agent
│   └── monitor-collector
├── v2.0.0/
│   ├── monitor-agent  
│   └── monitor-collector
├── v3.0.0/
│   ├── monitor-agent
│   └── monitor-collector
└── v4.0.0/  (future)
    ├── monitor-agent
    └── monitor-collector
```

## Deployment Commands

### Version Selection
```bash
# Deploy specific version
./deploy.py build --version v3.0.0
./deploy.py collector --version v3.0.0  
./deploy.py agent --version v3.0.0

# Deploy all with version
./deploy.py all --version v3.0.0

# List available versions
./deploy.py versions
```

### Version Information
```bash
# Check binary version
./target/release/monitor-agent --version
./target/release/monitor-collector --version

# Or from deployed binary
monitor-agent --version
# Output: monitor-agent 3.0.0
```

## Development Workflow

### Creating New Version

1. **Update version numbers:**
   ```bash
   # Update Cargo.toml workspace version
   version = "4.0.0"
   
   # Update deploy.py default
   UMA_VERSION = "v4.0.0"
   ```

2. **Build and test:**
   ```bash
   ./deploy.py build --version v4.0.0
   ./deploy.py collector --version v4.0.0
   ```

3. **Create git tag:**
   ```bash
   git tag v4.0.0
   git push origin v4.0.0
   ```

4. **Update CHANGELOG.md** with new features

### Backward Compatibility

**Configuration:** All versions use same config.toml format
- New fields are optional with sensible defaults
- Deprecated fields show warnings but still work

**Wire Protocol:** WebSocket message format is stable
- New fields added to JSON, never removed
- Version negotiation in Hello message

**Deployment:** Agents/collectors can run mixed versions
- v3 agents can connect to v4 collector
- v4 agents gracefully downgrade features for v3 collector

## Git Workflow

### Branch Strategy
```
main          - Latest stable (currently v3.0.0)
develop       - Next version development (v4.0.0)
release/v4.0.0 - Release preparation branches
hotfix/v3.0.1  - Patch releases for production
```

### Tag Strategy  
```
v1.0.0, v2.0.0, v3.0.0  - Major releases
v3.0.1, v3.0.2          - Patch releases
v4.0.0-beta.1           - Pre-releases
```

## Version Migration

### Agent Upgrade Process
1. Build new version: `./deploy.py build --version v4.0.0`
2. Stop old agents: `./deploy.py restart` 
3. Deploy new agents: `./deploy.py agent --version v4.0.0`
4. Verify connectivity in GUI

### Collector Upgrade Process
1. Deploy new collector: `./deploy.py collector --version v4.0.0`
2. Agents automatically reconnect with new features
3. Monitor for compatibility issues

### Rollback Process
```bash
# Emergency rollback to previous version
./deploy.py collector --version v3.0.0
./deploy.py agent --version v3.0.0
```

## Configuration Management

### Version-Specific Settings

**v3.0.0+ only:**
```toml
[agent]
maintenance = true          # v3+ only
debug_listen = "127.0.0.1:19100"  # v3+ only
```

**v2.0.0+ only:**
```toml
[[webhooks]]
kind = "slack"              # v2+ only
```

**All versions:**
```toml
[transport]
collector_url = "wss://collector:9443/v1/ingest"
[modules.thermal]
critical_temp = 75
```

## Monitoring Version Deployment

### Health Checks
```bash
# Check agent versions across fleet
./deploy.py status | grep version

# Check mixed version compatibility  
curl -s https://collector:8443/api/hosts | jq '.[] | {name, agent_version}'
```

### Version Drift Detection
The collector tracks agent versions and warns about:
- Agents running unsupported old versions
- Mixed version deployments (except during upgrades)  
- Version compatibility issues

## Troubleshooting

### Common Version Issues

**Problem:** Agent won't connect after upgrade
- **Solution:** Check certificate compatibility, regenerate if needed

**Problem:** Mixed versions causing issues  
- **Solution:** Complete fleet upgrade, don't run mixed versions long-term

**Problem:** New features not working
- **Solution:** Verify both agent and collector are correct version

### Debug Commands
```bash  
# Check what version is actually running
systemctl status monitor-agent
journalctl -u monitor-agent | grep "version\|starting"

# Verify binary version matches deployment
/usr/local/bin/monitor-agent --version
ls -la /usr/local/bin/monitor-*
```