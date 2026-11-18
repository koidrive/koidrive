#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fix {
    CreateFrom,
    RefusedKeeps,
    RefusedTakes,
    AbsentResent,
    Held,
    LostPosition,
    Carried,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rules {
    Spec,
    Main,
    RefusedAsMain,
    MoveResultEndsContent,
    MoveResultMarksContentResent,
    LandedResendIsOwn,
    ResendRecordEndsOwn,
    NodeAuthor,
    ClaimAtSeq,
    LossForgetsDeletedFields,
    DeleteSeesSynced,
    ResendRevivesSeenValue,
    DeleteDropsResends,
    DeleteNeedsSyncedLive,
    SeqDelete,
    SupersededDropsOwn,
    TrashSettlesDelete,
    DeleteSeesOnlyLocal,
    LossKeepsStaleSeq,
    OldServer,
    OldDeleter,
    DeletedAdoptsRecords,
    BoundOnly,
    ListedRemovalsInRecovery,
    RemovalTakesListedShape,
    HighestHeld,
    DeleteSeesOnlyView,
    BoundOnlyResendsSeen,
    TrashCoversView,
    EditSeesOnlyBase,
    LostCreateResends,
    NoLostPosition,
    AbsentDeleteResent,
    ResentCreateFrom,
    RefusedKeepsLostPosition,
    RefusedTakesLostPosition,
    CreateFixes,
    AllFixes,
    HeldDeleteResent,
    HeldFixes,
    HeldPositionFixes,
}

impl Rules {
    pub(super) fn main_rules(self) -> bool {
        matches!(self, Rules::Main | Rules::OldServer | Rules::OldDeleter)
    }

    pub(super) fn old_server(self) -> bool {
        matches!(self, Rules::OldServer | Rules::BoundOnlyResendsSeen)
    }

    pub(super) fn has(self, fix: Fix) -> bool {
        self.fixes().is_none_or(|fixes| fixes.contains(&fix))
    }

    fn fixes(self) -> Option<&'static [Fix]> {
        use Fix::*;
        Some(match self {
            Rules::Main | Rules::OldServer | Rules::OldDeleter => &[],
            Rules::ResentCreateFrom => &[CreateFrom],
            Rules::RefusedKeepsLostPosition => &[RefusedKeeps],
            Rules::RefusedTakesLostPosition => &[RefusedKeeps, RefusedTakes],
            Rules::CreateFixes => &[CreateFrom, RefusedKeeps, RefusedTakes],
            Rules::AbsentDeleteResent => &[AbsentResent],
            Rules::AllFixes => &[CreateFrom, RefusedKeeps, RefusedTakes, AbsentResent],
            Rules::HeldDeleteResent => &[AbsentResent, Held],
            Rules::HeldFixes => &[CreateFrom, RefusedKeeps, RefusedTakes, AbsentResent, Held],
            Rules::HeldPositionFixes => &[
                CreateFrom,
                RefusedKeeps,
                RefusedTakes,
                AbsentResent,
                Held,
                LostPosition,
            ],
            Rules::RefusedAsMain => &[CreateFrom, AbsentResent, Held, LostPosition, Carried],
            _ => return None,
        })
    }

    pub(super) fn checks_copies(self) -> bool {
        self.fixes().is_none() && !self.old_server()
    }

    pub(super) fn edits_see(self) -> bool {
        self.checks_copies() && self != Rules::EditSeesOnlyBase
    }

    pub(super) fn bound_only(self) -> bool {
        matches!(
            self,
            Rules::OldServer | Rules::BoundOnly | Rules::BoundOnlyResendsSeen
        )
    }
}
