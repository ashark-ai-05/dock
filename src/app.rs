use crate::model::{BoardFixture, Task, TaskState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    MoveDown,
    MoveUp,
    AcceptScope,
    RequestChanges,
    OpenLazygit,
}

#[derive(Debug)]
pub struct App {
    pub board: BoardFixture,
    pub selected: usize,
    pub notice: String,
    pub should_quit: bool,
}

impl App {
    pub fn new(board: BoardFixture) -> Self {
        let contract_state = if board.handoff_packet_for(0).validate().is_ok() {
            "Fixture handoff packet validated."
        } else {
            "Fixture handoff packet is invalid."
        };
        Self {
            board,
            selected: 0,
            notice: format!(
                "{} Dock records explicit decisions; it never infers completion.",
                contract_state
            ),
            should_quit: false,
        }
    }

    pub fn selected_task(&self) -> &Task {
        &self.board.tasks[self.selected]
    }

    pub fn apply(&mut self, action: Action) {
        match action {
            Action::MoveDown => self.selected = (self.selected + 1) % self.board.tasks.len(),
            Action::MoveUp => {
                self.selected =
                    (self.selected + self.board.tasks.len() - 1) % self.board.tasks.len()
            }
            Action::AcceptScope => {
                let task = &mut self.board.tasks[self.selected];
                if task.state == TaskState::NeedsInput {
                    task.state = TaskState::NeedsReview;
                    task.question = None;
                    self.notice = format!(
                        "{} moved to NEEDS REVIEW — decision recorded, not merged.",
                        task.id
                    );
                } else {
                    self.notice = "Accept scope only applies to a NEEDS INPUT handoff.".into();
                }
            }
            Action::RequestChanges => {
                let task = &mut self.board.tasks[self.selected];
                task.state = TaskState::ChangesRequested;
                self.notice = format!(
                    "{} routed back to {} with a changes-requested packet.",
                    task.id, task.agent
                );
            }
            Action::OpenLazygit => {
                let task = self.selected_task();
                self.notice = format!(
                    "Would open: cd {} && lazygit  (human Git operation; no automatic merge)",
                    task.worktree
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepting_scope_requires_a_needs_input_handoff() {
        let mut app = App::new(BoardFixture::example());
        app.apply(Action::AcceptScope);
        assert_eq!(app.selected_task().state, TaskState::NeedsReview);
        assert!(app.selected_task().question.is_none());
    }

    #[test]
    fn routing_back_never_claims_a_merge() {
        let mut app = App::new(BoardFixture::example());
        app.apply(Action::RequestChanges);
        assert_eq!(app.selected_task().state, TaskState::ChangesRequested);
        assert!(app.notice.contains("routed back"));
    }
}
