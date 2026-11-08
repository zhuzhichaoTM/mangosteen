# Changelog

All notable changes to the Mangosteen LLM Deployer Skill will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] - 2026-11-07

### Added
- Initial release of Mangosteen LLM Deployer Skill
- Core deployment workflows (local/cloud/mixed)
- Resource assessment functionality
- Model management (download/search/list/verify)
- Cloud provider configuration support
- OpenAI-compatible API integration
- Cross-platform support (macOS/Windows/Linux)
- Comprehensive documentation:
  - SKILL.md with complete workflows
  - references/commands.md (command quick reference)
  - references/resource-estimation.md (resource estimation formulas)
  - references/cloud-providers.md (cloud provider configuration)
  - references/troubleshooting.md (troubleshooting guide)
  - references/faq.md (frequently asked questions)
- Helper scripts:
  - scripts/detect-platform.sh (platform detection)
  - scripts/health-check.sh (environment health check)
  - scripts/prepare-env.sh (environment preparation)
  - scripts/quick-deploy.sh (quick deployment)
- QUICKSTART.md (5-minute quick start guide)
- Design documentation (DESIGN.md)
- Release metadata files (_meta.json, _skillhub_meta.json, skill-card.md)

### Features
- Local-first deployment strategy
- Automatic resource evaluation (VRAM/memory)
- Cloud elasticity with multi-provider support
- Four-dimensional cloud instance scoring (price/latency/availability/hardware)
- KV Cache-aware resource estimation
- GQA model optimization support
- SSH-based remote deployment
- Automatic library path injection for llama.cpp
- Model download with resume and SHA-256 verification
- Deployment ledger with automatic persistence
- Web UI support (optional)

### Documentation
- Complete command reference
- Resource estimation formulas with examples
- Cloud provider configuration templates
- Troubleshooting guide for common issues
- FAQ with 37+ questions
- Configuration best practices for different scenarios
- Quick start guide for beginners

### Security
- Credential security (environment variable/file/inline)
- Cost warnings for cloud deployment
- Automatic ledger tracking to prevent forgotten instances
- Credential masking in config show

## [Unreleased]

### Planned
- Icon design for skill marketplace
- Additional cloud provider templates
- More helper scripts
- Performance optimization guides
- Video tutorials
