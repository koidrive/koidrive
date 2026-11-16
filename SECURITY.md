# Security policy

## Reporting a vulnerability

Report vulnerabilities privately through GitHub's private vulnerability reporting. On the repository page, open the **Security and quality** tab and select **Report a vulnerability**.

Do not report a vulnerability in a public issue, pull request or comment. Never post exploit details or real user files in public. In the private report, use files, accounts and deployments that you created for testing.

Include the affected version or commit, the steps to reproduce the problem, and what an attacker could do with it.

## Supported versions

Koidrive is pre-1.0. Only the latest release and the `main` branch get security fixes.

[Dependabot](.github/dependabot.yml) groups security updates for Cargo, Python build tools, and GitHub Actions; routine version-update pull requests are disabled.

## What to expect

A maintainer acknowledges your report and replies in the report on GitHub. You get either a fix or an explanation of why we will not change the behavior.
