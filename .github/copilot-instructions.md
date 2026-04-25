# Git Workflow Rules
- After every task I ask you to complete, generate the specific Git command string I need to run.
- Format: `git add . && git commit -m "feat: [specific task description]" && git push origin main`
- Always run this command after completing a task that modifies the codebase, but do run it for documentation or instruction file edits too. But don't run it for changes for files that are in the gitignore & ignored by Git 
- Do not ask me if I want to commit; just provide the command as the final step.
- Do not push when I ask a simple question about the project or for code snippets. Only generate the command after completing a task that modifies the codebase. Do not push when editing documentation or instructions files. Push when editing bat & README files. Push when editing .gitignore. Push when editing LICENSE.
- PUSH THE CHANGES YOURSELF USING COMMANDS, NEVER RELY ON ME TO PUSH