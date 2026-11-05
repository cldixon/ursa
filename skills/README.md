# Agent skills

Skills that teach a coding agent to use Ursa. Each skill is a folder with a `SKILL.md`: YAML frontmatter that says when to use it, then instructions.

| Skill | Use it to |
| --- | --- |
| [`ursa-sql`](ursa-sql/SKILL.md) | Write and run SQL for Ursa's SQL engine and the `ursa` command-line tool. |

## Install

The agent also needs the `ursa` binary on its `PATH`: `cargo install ursa-cli`, or a binary from the [releases page](https://github.com/cldixon/ursa/releases).

**Claude Code.** Copy the folder into your skills directory, for one project or for all of them:

```bash
# For one project: run in the project's root.
mkdir -p .claude/skills && cp -r path/to/ursa/skills/ursa-sql .claude/skills/

# For every project.
mkdir -p ~/.claude/skills && cp -r path/to/ursa/skills/ursa-sql ~/.claude/skills/
```

Claude Code finds the skill by its description and loads it when a task calls for Ursa SQL. To check, ask it "what skills do you have?".

**Claude apps.** Zip the folder (`zip -r ursa-sql.zip ursa-sql`) and upload it under Settings → Capabilities → Skills.

**Other agents.** `SKILL.md` is plain Markdown. Add it to the agent's instructions or context.

## Testing

Every SQL example in a skill runs in CI against the `ursa` binary, and its output is checked where the skill shows it: `cargo test -p ursa-cli --test docs skill`. The convention is the one the docs use; it is described at the top of `ursa-cli/tests/docs.rs`.
