// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
#[ignore = "a delete comes undone across server restores around a replaced folder (GUN-767)"]
fn a_delete_stays_across_server_restores_around_a_replaced_folder() {
    use Cut::*;
    use Outcome::*;
    use Reply::*;
    use Step::*;
    converge(
        2,
        &[
            vec![
                (
                    0,
                    User(Action {
                        kind: 2,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (
                    2,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 2,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 3,
                        target: 2470589270640886874,
                        parent: 0,
                        name: 9,
                        text: 0,
                    }),
                ),
                (
                    1,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 2,
                        text: 0,
                    }),
                ),
                (
                    1,
                    User(Action {
                        kind: 3,
                        target: 0,
                        parent: 0,
                        name: 7,
                        text: 0,
                    }),
                ),
            ],
            vec![
                (
                    0,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 4,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 2,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 3,
                        target: 861725947464577428,
                        parent: 0,
                        name: 3,
                        text: 0,
                    }),
                ),
                (
                    1,
                    User(Action {
                        kind: 3,
                        target: 1809117414448709468,
                        parent: 0,
                        name: 5,
                        text: 0,
                    }),
                ),
                (0, Work(0)),
                (0, Work(0)),
                (0, Work(2488612757522)),
                (1, Work(10710087073685529507)),
                (0, Work(12415476291504558141)),
                (
                    0,
                    User(Action {
                        kind: 1,
                        target: 8977249278648222204,
                        parent: 14985898319401055985,
                        name: 1,
                        text: 2,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 2,
                        target: 10617578100218958860,
                        parent: 109209292269612126,
                        name: 1,
                        text: 3,
                    }),
                ),
                (2, Deliver(8471077902145496926)),
                (0, Commit(NoChunks)),
                (2, Backup),
            ],
            vec![
                (1, Work(11789011912308573104)),
                (1, Effect(Run)),
                (0, Restore),
                (1, Effect(Outcome::Crash(Placed))),
                (
                    1,
                    User(Action {
                        kind: 3,
                        target: 11093896227811117673,
                        parent: 13838485866651207847,
                        name: 7,
                        text: 2,
                    }),
                ),
                (0, Connect),
                (
                    1,
                    User(Action {
                        kind: 2,
                        target: 11203304110497521370,
                        parent: 14259164623482894290,
                        name: 3,
                        text: 2,
                    }),
                ),
                (0, Work(5545155993445983427)),
                (1, Work(5494100873526995747)),
                (2, Backup),
                (0, Connect),
                (2, Work(7378848995949163628)),
                (2, Deliver(12748348932081378684)),
                (1, Commit(Served)),
                (2, Work(17866253982854176093)),
                (2, Connect),
                (2, Work(7349727060586564346)),
                (2, Connect),
                (0, Work(16283421099575165932)),
                (
                    0,
                    User(Action {
                        kind: 2,
                        target: 14337819998312701825,
                        parent: 8095654488242736789,
                        name: 8,
                        text: 3,
                    }),
                ),
                (0, Effect(Run)),
                (0, Connect),
                (2, Effect(Run)),
                (0, Backup),
                (0, Scan),
                (
                    2,
                    User(Action {
                        kind: 4,
                        target: 15883777944504750755,
                        parent: 14444554943983268693,
                        name: 1,
                        text: 2,
                    }),
                ),
                (1, Restore),
                (2, Work(6267074935625447998)),
                (0, Work(9842791980092395285)),
                (
                    0,
                    User(Action {
                        kind: 2,
                        target: 15253874012712831777,
                        parent: 1558576738387900523,
                        name: 9,
                        text: 2,
                    }),
                ),
                (0, Work(9644443201055046124)),
                (2, Commit(Served)),
                (0, Effect(Run)),
                (1, Backup),
                (0, Scan),
            ],
        ],
    )
    .unwrap();
}

#[test]
#[ignore = "a delete comes undone across server restores and rewinds (GUN-917)"]
fn a_delete_stays_across_server_restores_and_rewinds() {
    use Reply::*;
    use Step::*;
    converge(
        3,
        &[
            vec![
                (
                    1,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (
                    1,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 2,
                        text: 0,
                    }),
                ),
                (
                    1,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 3,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 1,
                        text: 0,
                    }),
                ),
            ],
            vec![
                (0, Backup),
                (
                    1,
                    User(Action {
                        kind: 2,
                        target: 12825638307164173158,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
            ],
            vec![
                (0, Restore),
                (
                    1,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 29226930420878976,
                        name: 4,
                        text: 0,
                    }),
                ),
                (2, Connect),
                (2, Work(0)),
                (2, Commit(Served)),
                (2, Work(9005262372980157177)),
                (2, Work(1165962725008932860)),
                (
                    1,
                    User(Action {
                        kind: 3,
                        target: 16404949188246174543,
                        parent: 16571246819194558495,
                        name: 0,
                        text: 0,
                    }),
                ),
                (
                    1,
                    User(Action {
                        kind: 4,
                        target: 9520376179847244920,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (0, Restore),
            ],
        ],
    )
    .unwrap();
}

#[test]
#[ignore = "a device that never gave a folder its pending name finishes it after the folder is deleted and brought back (GUN-755)"]
fn only_the_namer_finishes_a_pending_name_of_a_folder_deleted_and_brought_back() {
    use Reply::*;
    use Step::*;
    converge(
        3,
        &[
            vec![(
                0,
                User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            )],
            vec![
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 1,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 3,
                        target: 0,
                        parent: 1,
                        name: 0,
                        text: 0,
                    }),
                ),
                (0, Work(0)),
                (0, Work(0)),
                (0, Work(0)),
                (0, Work(0)),
                (0, Work(0)),
                (1, Work(0)),
                (1, Work(0)),
                (1, Work(0)),
                (1, Work(0)),
                (1, Work(0)),
                (0, Commit(Refused(error::Code::RetryLater))),
                (
                    1,
                    User(Action {
                        kind: 4,
                        target: 1,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (1, Scan),
                (1, Commit(Served)),
                (1, Commit(Served)),
                (1, Deliver(0)),
                (
                    0,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 1,
                        name: 2,
                        text: 0,
                    }),
                ),
                (0, Scan),
                (0, Commit(Served)),
                (0, Commit(Served)),
                (0, Deliver(0)),
                (
                    1,
                    User(Action {
                        kind: 4,
                        target: 1,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (
                    2,
                    User(Action {
                        kind: 4,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (2, Deliver(3568239211836307098)),
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (2, Work(14002780067840989677)),
                (2, Work(1147192969390414053)),
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (
                    2,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 1192255131271792001,
                        name: 2,
                        text: 0,
                    }),
                ),
                (2, Work(6036150968848666325)),
                (2, Work(1548992151789521797)),
                (2, Work(346916552495723481)),
            ],
        ],
    )
    .unwrap();
}
