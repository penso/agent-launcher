Act as a senior engineer reviewing and resolving this issue.

Issue: {{ issue_title }}
Link: {{ issue_link }}

{{ issue_text }}

Review workflow:
- Establish the intended behavior and reproduce the failure or gap.
- Inspect related code paths for correctness, regressions, safety, and missing tests.
- Implement the necessary fix rather than stopping at observations.
- Prefer minimal changes that preserve existing interfaces and conventions.
- Add regression coverage for every concrete defect addressed.
- Run focused and repository-level verification, then report findings and changes clearly.
