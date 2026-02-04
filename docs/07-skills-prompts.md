# Skills & Custom Prompts

## Overview

Codex supports "skills" - custom prompt injections loaded from `SKILL.md` files. Skills allow users and repositories to define reusable instructions, specialized behaviors, and tool configurations that get injected into the conversation when invoked.

## Key Components

| Component | File | Purpose |
|-----------|------|---------|
| `SkillsManager` | `skills/manager.rs` | Loads and caches skills per working directory |
| `SkillMetadata` | `skills/model.rs` | Skill definition structure |
| `SkillInjection` | `skills/injection.rs` | Injects skills into conversation |
| `SkillLoader` | `skills/loader.rs` | Parses SKILL.md files |
| `SkillScope` | `skills/model.rs` | User, Repo, or System scope |

## Skill File Format

Skills are defined in `SKILL.md` files with YAML frontmatter:

```markdown
---
name: commit
description: Create a git commit with a well-formatted message
allowed_tools:
  - shell
  - read_file
mcp_tools:
  - server: git
    tool: status
---

# Commit Instructions

When creating a commit:

1. First run `git status` to see changed files
2. Run `git diff --staged` to review staged changes
3. Write a commit message following conventional commits format
4. Use imperative mood in the subject line

## Format

```
<type>(<scope>): <subject>

<body>
```

Types: feat, fix, docs, style, refactor, test, chore
```

### Frontmatter Fields

```rust
struct SkillMetadata {
    /// Display name (required)
    name: String,

    /// Short description for UI/help
    description: Option<String>,

    /// How skill is invoked (cli, user, auto)
    interface: Option<SkillInterface>,

    /// Required MCP tool dependencies
    mcp_tools: Option<Vec<McpToolDep>>,

    /// Allowed built-in tools
    allowed_tools: Option<Vec<String>>,

    /// File path (populated during loading)
    path: Option<PathBuf>,

    /// Where skill was found
    scope: Option<SkillScope>,
}

struct McpToolDep {
    server: String,
    tool: String,
}

enum SkillInterface {
    Cli,      // Invoked via CLI flag
    User,     // Invoked by user mention ($skill)
    Auto,     // Automatically injected
}
```

## Skill Scopes

Skills are discovered from multiple locations with priority:

```rust
enum SkillScope {
    User,    // ~/.config/codex/skills/ or ~/.codex/skills/
    Repo,    // .codex/skills/ in repository
    System,  // Built-in skills
}
```

### Discovery Order

```
1. User skills (highest priority)
   ~/.config/codex/skills/*.md
   ~/.codex/skills/*.md

2. Repository skills
   <repo>/.codex/skills/*.md

3. System/built-in skills (lowest priority)
   Embedded in binary
```

When multiple skills have the same name, higher-priority scopes win.

## Skill Discovery

### Directory Scanning

```rust
const MAX_SCAN_DEPTH: usize = 3;  // Max directory depth to scan

fn discover_skills(cwd: &Path) -> Vec<SkillMetadata> {
    let mut skills = Vec::new();

    // 1. Scan user config directories
    for user_dir in get_user_skill_dirs() {
        skills.extend(scan_directory(&user_dir, SkillScope::User));
    }

    // 2. Scan repository .codex/skills/
    if let Some(repo_root) = find_repo_root(cwd) {
        let repo_skills_dir = repo_root.join(".codex/skills");
        skills.extend(scan_directory(&repo_skills_dir, SkillScope::Repo));
    }

    // 3. Add system skills
    skills.extend(get_system_skills());

    // Deduplicate by name (first wins)
    deduplicate_by_name(skills)
}
```

### File Pattern

Skills must be named `SKILL.md` or `*.skill.md`:

```
.codex/skills/
├── commit.skill.md
├── review.skill.md
└── test/
    └── SKILL.md      # Named by directory
```

## SkillsManager

Manages skill loading with per-directory caching:

```rust
struct SkillsManager {
    /// Cache keyed by working directory
    cache: HashMap<PathBuf, CachedSkills>,
}

struct CachedSkills {
    skills: Vec<SkillMetadata>,
    loaded_at: Instant,
}

impl SkillsManager {
    /// Get skills for a working directory (cached)
    fn get_skills(&mut self, cwd: &Path) -> &[SkillMetadata] {
        if !self.cache.contains_key(cwd) || self.is_stale(cwd) {
            let skills = discover_skills(cwd);
            self.cache.insert(cwd.to_path_buf(), CachedSkills {
                skills,
                loaded_at: Instant::now(),
            });
        }
        &self.cache[cwd].skills
    }

    /// List all available skill names
    fn list_skill_names(&mut self, cwd: &Path) -> Vec<String> {
        self.get_skills(cwd)
            .iter()
            .map(|s| s.name.clone())
            .collect()
    }

    /// Get a specific skill by name
    fn get_skill(&mut self, cwd: &Path, name: &str) -> Option<&SkillMetadata> {
        self.get_skills(cwd)
            .iter()
            .find(|s| s.name == name)
    }
}
```

## Skill Invocation

### Explicit Mention Detection

Users invoke skills by mentioning them with `$` prefix:

```rust
const SKILL_MENTION_PATTERN: &str = r"\$([a-zA-Z][a-zA-Z0-9_-]*)";

fn collect_explicit_skill_mentions(text: &str) -> Vec<String> {
    let re = Regex::new(SKILL_MENTION_PATTERN).unwrap();
    re.captures_iter(text)
        .map(|cap| cap[1].to_string())
        .collect()
}

// Example: "Please $commit my changes and $review the PR"
// Returns: ["commit", "review"]
```

### Tool Mention Detection

Skills can also be triggered by tool mentions:

```rust
fn extract_tool_mentions(text: &str) -> Vec<String> {
    // Detect tool-like patterns in user input
    // e.g., "run shell command", "read the file"
}
```

## Skill Injection

### Building Injections

When skills are invoked, their content is injected into the conversation:

```rust
fn build_skill_injections(
    skills_manager: &mut SkillsManager,
    cwd: &Path,
    user_input: &str,
) -> Vec<SkillInjection> {
    let mut injections = Vec::new();

    // 1. Find explicitly mentioned skills
    let mentions = collect_explicit_skill_mentions(user_input);

    for skill_name in mentions {
        if let Some(skill) = skills_manager.get_skill(cwd, &skill_name) {
            // Load skill content from file
            let content = load_skill_content(&skill.path)?;

            injections.push(SkillInjection {
                name: skill.name.clone(),
                content,
                source: skill.scope.clone(),
            });
        }
    }

    // 2. Add auto-injected skills
    for skill in skills_manager.get_skills(cwd) {
        if skill.interface == Some(SkillInterface::Auto) {
            let content = load_skill_content(&skill.path)?;
            injections.push(SkillInjection {
                name: skill.name.clone(),
                content,
                source: skill.scope.clone(),
            });
        }
    }

    injections
}

struct SkillInjection {
    name: String,
    content: String,
    source: SkillScope,
}
```

### Injection into Conversation

Skills are injected as system-like messages before the user's input:

```rust
fn inject_skills_into_history(
    history: &mut Vec<ResponseItem>,
    injections: &[SkillInjection],
) {
    for injection in injections {
        // Create a skill instruction item
        let skill_item = ResponseItem::SkillInstructions {
            name: injection.name.clone(),
            content: format!(
                "<skill name=\"{}\">\n{}\n</skill>",
                injection.name,
                injection.content
            ),
        };

        // Insert before the latest user message
        let insert_pos = find_last_user_message_index(history);
        history.insert(insert_pos, skill_item);
    }
}
```

### Injection Format

Skills appear in the conversation as tagged blocks:

```xml
<skill name="commit">
# Commit Instructions

When creating a commit:
1. First run `git status` to see changed files
...
</skill>
```

## Skill Dependencies

### MCP Tool Dependencies

Skills can declare required MCP tools:

```yaml
---
name: database-query
mcp_tools:
  - server: postgres
    tool: query
  - server: postgres
    tool: list_tables
---
```

When a skill with MCP dependencies is invoked:

```rust
fn ensure_skill_dependencies(
    skill: &SkillMetadata,
    mcp_manager: &McpConnectionManager,
) -> Result<()> {
    if let Some(deps) = &skill.mcp_tools {
        for dep in deps {
            // Verify MCP server is connected
            if !mcp_manager.is_server_connected(&dep.server) {
                return Err(SkillError::MissingMcpServer(dep.server.clone()));
            }

            // Verify tool is available
            let tools = mcp_manager.list_tools(&dep.server)?;
            if !tools.contains(&dep.tool) {
                return Err(SkillError::MissingMcpTool {
                    server: dep.server.clone(),
                    tool: dep.tool.clone(),
                });
            }
        }
    }
    Ok(())
}
```

### Allowed Tools

Skills can restrict which tools the agent may use:

```yaml
---
name: safe-review
allowed_tools:
  - read_file
  - search
  # shell is NOT allowed - read-only review
---
```

## Built-in Skills

System skills embedded in the binary:

| Skill | Purpose |
|-------|---------|
| `commit` | Git commit with conventional format |
| `pr` | Create/update pull requests |
| `review` | Code review workflow |
| `test` | Run and analyze tests |
| `debug` | Debugging assistance |

## Skill Loading Flow

```
┌─────────────────────────────────────────────────────────────────────────┐
│                        User Input                                        │
│                                                                          │
│  "Please $commit my changes"                                             │
│                                                                          │
└─────────────────────────────────┬───────────────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  collect_explicit_skill_mentions()                                       │
│                                                                          │
│  Pattern: \$([a-zA-Z][a-zA-Z0-9_-]*)                                     │
│  Result: ["commit"]                                                      │
│                                                                          │
└─────────────────────────────────┬───────────────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  SkillsManager::get_skill("commit")                                      │
│                                                                          │
│  1. Check cache for cwd                                                  │
│  2. If miss: discover_skills(cwd)                                        │
│  3. Return matching skill metadata                                       │
│                                                                          │
└─────────────────────────────────┬───────────────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  load_skill_content(skill.path)                                          │
│                                                                          │
│  1. Read SKILL.md file                                                   │
│  2. Parse YAML frontmatter                                               │
│  3. Extract markdown body                                                │
│                                                                          │
└─────────────────────────────────┬───────────────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  ensure_skill_dependencies()                                             │
│                                                                          │
│  - Verify MCP servers connected                                          │
│  - Verify required tools available                                       │
│                                                                          │
└─────────────────────────────────┬───────────────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  inject_skills_into_history()                                            │
│                                                                          │
│  Insert <skill> block before user message                                │
│                                                                          │
└─────────────────────────────────────────────────────────────────────────┘
```

## Skill Content Processing

### Frontmatter Parsing

```rust
fn parse_skill_file(content: &str) -> Result<(SkillMetadata, String)> {
    // Check for YAML frontmatter
    if !content.starts_with("---") {
        return Err(SkillError::MissingFrontmatter);
    }

    // Find end of frontmatter
    let end = content[3..]
        .find("---")
        .ok_or(SkillError::InvalidFrontmatter)?;

    let yaml = &content[3..end + 3];
    let body = &content[end + 6..];  // Skip closing ---

    let metadata: SkillMetadata = serde_yaml::from_str(yaml)?;

    Ok((metadata, body.trim().to_string()))
}
```

### Variable Substitution

Skills can include variables that get substituted at injection time:

```markdown
---
name: context
---

Current working directory: {{cwd}}
Current branch: {{git_branch}}
User: {{user}}
```

```rust
fn substitute_variables(content: &str, context: &SkillContext) -> String {
    content
        .replace("{{cwd}}", &context.cwd.display().to_string())
        .replace("{{git_branch}}", &context.git_branch.unwrap_or_default())
        .replace("{{user}}", &context.user)
}
```

## Error Handling

```rust
enum SkillError {
    /// Skill file not found
    NotFound(String),

    /// Invalid YAML frontmatter
    InvalidFrontmatter,

    /// Missing required frontmatter
    MissingFrontmatter,

    /// Required MCP server not connected
    MissingMcpServer(String),

    /// Required MCP tool not available
    MissingMcpTool { server: String, tool: String },

    /// Skill content too large
    ContentTooLarge { name: String, size: usize },
}
```

Errors are reported to the user but don't block the conversation:

```rust
fn handle_skill_error(err: SkillError) -> Option<String> {
    match err {
        SkillError::NotFound(name) => {
            Some(format!("Skill '{}' not found. Available skills: ...", name))
        }
        SkillError::MissingMcpServer(server) => {
            Some(format!("Skill requires MCP server '{}' which is not connected", server))
        }
        _ => None,  // Silent failure for some errors
    }
}
```

## Deeper Investigation Pointers

| Topic | Where to Look |
|-------|---------------|
| Skill discovery | `skills/loader.rs` |
| Skill caching | `skills/manager.rs` |
| Mention detection | `skills/injection.rs` |
| Skill metadata | `skills/model.rs` |
| Built-in skills | `skills/builtin/` |
| Frontmatter parsing | `skills/loader.rs` |
