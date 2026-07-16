//! Mining source-kind / detector ids, answer-banner separators, and confidence floors.
//! Hand-owned; mirrored to Python via `_native.embedded_literals()`.

pub const TRANSCRIPT_MESSAGE: &str = "transcript_message";
pub const PLAN_REVIEW: &str = "plan_review";
pub const INTERRUPT_REJECTION: &str = "interrupt_rejection";
pub const REVIEW_COMMENT: &str = "review_comment";
pub const QUESTION_ANSWER: &str = "question_answer";
pub const DETECTOR_TRANSCRIPT_MESSAGE: &str = "transcript_message";
pub const DETECTOR_EXIT_PLAN_REJECTION: &str = "exit_plan_rejection";
pub const DETECTOR_PLAN_REENTRY: &str = "plan_reentry";
pub const DETECTOR_DENIAL: &str = "denial";
pub const DETECTOR_INTERRUPT: &str = "interrupt";
pub const DETECTOR_REVIEW_COMMENT: &str = "review_comment";
pub const DETECTOR_ASK_USER_QUESTION: &str = "ask_user_question";
pub const ANSWER_PREVIEW_SEP: &str = " selected preview:\n";
pub const ANSWER_NOTES_SEP: &str = " notes: ";
pub const NO_OPTION_SELECTED: &str = "(no option selected)";
pub const NONE: f64 = 0.0;
pub const LOW: f64 = 0.25;
