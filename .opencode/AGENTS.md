#  AI Agent Execution Protocol (AGENTS.md)

This file dictates the strict operational boundaries, version control procedures, and git commit cycles for all AI Agents, Code Assistants, and LLM Engines operating on this codebase.

---

## 1. Core Mandate: Atomic Commits

* **Never lump changes together:** Every distinct feature, refactor, optimization, or bug fix must be isolated into its own granular, atomic git commit.
* **Commit Message Format:** Follow the Conventional Commits specification (e.g., `feat(protocol): add v26_1 handshake packet packet parsing`, `perf(simd): optimize chunk compression pipeline`).

---

## 2. Version Control & Synchronization Rules

Agents must monitor the prompt instructions and follow these strict execution branches:

### Branch A: Explicit "Push" Request
* **Trigger:** The user explicitly asks to "push changes", "sync repo", or similar variations in the prompt.
* **Action:** Stage all modified files, commit them immediately using the correct format, and execute `git push`.

### Branch B: Auto-Commit & Push After Every Change
* **Trigger:** The user instructs the agent to make changes and "commit & push" after each one, or any variation requesting automatic version control after each change.
* **Action:**
  1. Perform the requested task and create a local commit.
  2. Immediately execute `git push` after each commit without waiting for a user prompt.
  3. Track via `git cherry -v` or `git log --oneline` to confirm each push succeeds.

---

## 3. Pre-Commit Verification

Before running `git commit`, the agent must verify:
1. `cargo check` passes with zero errors on the nightly toolchain.
2. The codebase remains `no_std` and `no_alloc` compliant where applicable.

Failure to follow these synchronization directives will result in operational execution failure.