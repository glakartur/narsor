# GitHub Copilot SDK Adapter - Skills Integration Examples

## Overview
This document demonstrates how to use skills with the GitHub Copilot adapter in Narsor workflows.

## Understanding Skills

Skills in GitHub Copilot SDK are domain-specific instruction sets that enhance the agent's capabilities. They come in two forms:

1. **System Prompt Skills** - Instructions embedded directly in the prompt
2. **Skill Files** - `SKILL.md` files in structured skill directories

## Current Implementation: System Prompt Skills

The GitHub Copilot adapter supports skills immediately using the `append_system_prompt` parameter. This is the simplest approach that requires no additional wrapper configuration.

### Basic Example

```yaml
analyze_code:
  action:
    type: agent
    harness: github-copilot
    model: gpt-4
    prompt: |
      Review the following code for issues:
      {{ inputs.code_snippet }}
    parameters:
      append_system_prompt: |
        # Code Review Skill
        You are an expert code reviewer with 10 years of experience.
        
        Focus on:
        1. Security vulnerabilities (SQL injection, XSS, CSRF, etc.)
        2. Performance implications (N+1 queries, unnecessary loops, etc.)
        3. Error handling and resilience
        4. Code style and maintainability
        
        Format your review with:
        - CRITICAL issues first
        - Then WARNINGS
        - Finally SUGGESTIONS for improvement
```

### Multi-Skill Example

```yaml
comprehensive_analysis:
  action:
    type: agent
    harness: github-copilot
    model: gpt-4
    prompt: |
      Perform comprehensive analysis on: {{ inputs.target_file }}
    parameters:
      append_system_prompt: |
        # Security Audit Skill
        Check for:
        - Authentication and authorization issues
        - Data exposure risks
        - Injection vulnerabilities
        - Cryptographic weaknesses
        
        # Performance Analysis Skill
        Identify:
        - Inefficient database queries
        - Memory leaks
        - Unnecessary allocations
        - Slow algorithms
        
        # Maintainability Skill
        Evaluate:
        - Code complexity and readability
        - Test coverage adequacy
        - Documentation completeness
        - Architectural consistency
```

### Integration with Environment Settings

You can combine environment skills with workflow-specific guidance:

```yaml
review_pull_request:
  action:
    type: agent
    harness: github-copilot
    prompt: |
      Review this pull request and provide feedback:
      {{ inputs.pr_diff }}
    parameters:
      append_system_prompt: |
        # Company Code Review Standards
        
        Our standards require:
        - All public APIs documented with examples
        - Minimum 80% test coverage
        - No hardcoded credentials or secrets
        - Performance regression must be < 5%
        
        Also check the guidelines in our skills directory,
        which provide domain-specific review criteria.
```

## Advanced Configuration

### Using Model-Specific Skills

Different models may benefit from different skill formulations:

```yaml
analyze_with_claude:
  action:
    type: agent
    harness: github-copilot
    model: claude-opus
    prompt: Analyze this issue
    parameters:
      append_system_prompt: |
        # Claude-Optimized Analysis Skill
        Think step-by-step through the problem space.
        Consider multiple perspectives and edge cases.
        Show your reasoning process clearly.

analyze_with_gpt:
  action:
    type: agent
    harness: github-copilot
    model: gpt-4
    prompt: Analyze this issue
    parameters:
      append_system_prompt: |
        # GPT-Optimized Analysis Skill
        Structure your analysis with clear headings.
        Provide specific examples for each point.
        Rank findings by importance.
```

### Conditional Skills

```yaml
context:
  environment: production

states:
  validate_code:
    action:
      type: agent
      harness: github-copilot
      prompt: |
        Validate this code:
        {{ inputs.code }}
      parameters:
        append_system_prompt: |
          # Production Code Validation Skill
          For production code, enforce:
          - Zero tolerance for SQL injection risks
          - Comprehensive error handling
          - Proper resource cleanup
          - Logging at appropriate levels
          - Rate limiting awareness
```

## Future: Skills from Environment

When the wrapper CLI is extended, you'll be able to reference skills from the environment directly:

```yaml
# This is a future enhancement
analyze_with_env_skills:
  action:
    type: agent
    harness: github-copilot
    prompt: Analyze the codebase
    # In the future:
    # skills:
    #   - code-review
    #   - security-audit
    #   - performance-analysis
```

## Best Practices

1. **Keep Skills Focused**: Each skill should address a specific domain or aspect
2. **Make Skills Reusable**: Write skills that can be shared across actions
3. **Version Your Skills**: Update skills with the workflow when making changes
4. **Document Expectations**: Clearly state what the skill will evaluate
5. **Test Skills**: Validate that skills produce the expected output
6. **Use Examples**: Include examples in skill descriptions when helpful

## Integration with Narsor Workflow Engine

```yaml
workflow:
  version: 1
  description: Code Review and Quality Gate

states:
  # State 1: Initial Review
  code_review:
    action:
      type: agent
      harness: github-copilot
      prompt: |
        Review the PR changes:
        {{ inputs.pr_changes }}
      parameters:
        append_system_prompt: |
          # PR Review Skill
          Provide structured feedback on:
          1. Code quality
          2. Security concerns
          3. Performance impact
          4. API contract changes
    on_success: quality_check
    on_failure: notify_author

  # State 2: Quality Metrics
  quality_check:
    action:
      type: agent
      harness: github-copilot
      prompt: |
        Check test coverage for:
        {{ inputs.coverage_report }}
      parameters:
        append_system_prompt: |
          # Test Coverage Skill
          Evaluate if test coverage meets:
          - Critical paths: 100%
          - Main features: 90%
          - Utils/helpers: 80%
    on_success: approved
    on_failure: needs_improvement

  approved:
    action:
      type: human
      prompt: |
        PR has passed all quality checks.
        Ready to merge? (approve/reject)

  needs_improvement:
    action:
      type: human
      prompt: |
        Code needs improvement based on review.
        Would you like to revise? (yes/no)
```

## Troubleshooting

### Skill Not Being Used
- Check that `append_system_prompt` parameter is correctly spelled
- Verify the skill text is valid Markdown or plain text
- Ensure the prompt itself isn't overriding the skill guidance

### Conflicting Instructions
- If the main prompt and skill contradict, the model may be confused
- Use clear, non-conflicting instructions
- Test with a simpler skill first to isolate issues

### Performance Issues
- Very long skills can increase latency
- Consider splitting complex skills into multiple actions
- Use concise, clear language in skill descriptions

## See Also

- [GitHub Copilot SDK Documentation](https://docs.github.com/en/copilot/how-tos/copilot-sdk)
- [Copilot SDK Skills Documentation](https://github.com/github/copilot-sdk/blob/main/docs/features/skills.md)
- [Narsor Workflow Schema](../dev/workflow-schema.md)
- [GitHub Copilot SDK Main Documentation](./github_copilot_sdk.md)
