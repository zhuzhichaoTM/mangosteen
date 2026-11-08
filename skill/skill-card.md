## Description: <br>
云端大模型部署客户端：自动选择租赁多家云厂商GPU算力资源，最优化选择GPU节点，自动化部署开源大模型。评估模型所需的GPU资源，从多家云厂商中选择最优节点，自动化部署，提供 OpenAI 兼容接口。 <br>

This skill is ready for commercial/non-commercial use. <br>

## Publisher: <br>
[朱志超](https://github.com/zhuzhichaoTM) <br>

### License/Terms of Use: <br>
Apache-2.0 <br>

## Use Case: <br>
Developers and researchers use this skill to deploy large language models with cloud GPU elasticity. It automatically evaluates VRAM/memory needs, selects optimal GPU nodes from multiple cloud providers (price, latency, availability, hardware scoring), deploys models via SSH tunneling, and provides OpenAI-compatible API. Suitable for quick deployment of LLM inference services on macOS/Windows/Linux. <br>

### Deployment Geography for Use: <br>
Global <br>

## Known Risks and Mitigations: <br>
Risk: Cloud instances incur hourly charges; forgetting to stop instances may lead to unexpected costs. <br>
Mitigation: The skill provides explicit cost warnings and automated ledger tracking. Users should monitor deployment status with `mangosteen cloud instances` and stop unused instances with `mangosteen stop --all`. <br>

Risk: Cloud provider credentials may be exposed if misconfigured. <br>
Mitigation: Credentials support environment variables and file-based storage. The skill never displays credentials in plaintext. Users should follow credential security best practices. <br>

Risk: Cloud GPU inference may have network latency. <br>
Mitigation: The skill selects low-latency nodes based on geographic proximity and network conditions. <br>

## Reference(s): <br>
- [Mangosteen GitHub Repository](https://github.com/zhuzhichaoTM/mangosteen) <br>
- [Mangosteen Documentation](https://github.com/zhuzhichaoTM/mangosteen#readme) <br>

## Skill Output: <br>
**Output Type(s):** [Text, Markdown, Configuration] <br>
**Output Format:** [Command outputs, deployment status, configuration files] <br>
**Output Parameters:** [1D] <br>
**Other Properties Related to Output:** [Outputs include deployment status, resource assessment results, node scoring details, and configuration recommendations for human review before execution.] <br>

## Skill Version(s): <br>
1.0.0 (source: skill frontmatter) <br>

## Ethical Considerations: <br>
Users should evaluate whether this skill is appropriate for their environment, review any generated or modified configuration files before applying them, and apply their organization's security, compliance, and cost management requirements before deploying to cloud providers. Cloud deployment may incur costs; users should monitor usage and costs. <br>
