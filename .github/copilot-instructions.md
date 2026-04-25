# Git Workflow Rules
- After every task I ask you to complete, generate the specific Git command string I need to run.
- Format: `git add . && git commit -m "feat: [specific task description]" && git push origin main`
- Always run this command at the end of every task after you finish it.
- Do not ask me if I want to commit; just provide the command as the final step.
- Do not push when I ask a simple question about the project or for code snippets. Only generate the command after completing a task that modifies the codebase. Do not push when editing documentation or instructions files. Push when editing bat & README files. Push when editing .gitignore. Push when editing LICENSE.