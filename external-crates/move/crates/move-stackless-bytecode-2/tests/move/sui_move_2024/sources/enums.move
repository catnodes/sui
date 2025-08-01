// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

module sui_move_2024::action {

    public enum Action has drop {
        Stop,
        MoveTo { x: u64, y: u64 },
        ChangeSpeed(u64),
    }

    public fun test() {
        // Define a list of actions
        let actions: vector<Action> = vector[
            Action::MoveTo { x: 10, y: 20 },
            Action::ChangeSpeed(40),
            Action::MoveTo { x: 10, y: 20 },
            Action::Stop,
            Action::ChangeSpeed(40),
        ];

        let mut total_moves = 0;

        'loop_label: loop {
            let mut i = 0;
            while (i < actions.length()) {
                let action = &actions[i];

                match (action) {
                    Action::MoveTo { x, y } => {
                        'loop_label: loop {
                            total_moves = total_moves + *x + *y;
                            break 'loop_label
                        };
                    },
                    Action::ChangeSpeed(speed) => {
                        'loop_label: loop {
                            total_moves = total_moves + *speed;
                            break 'loop_label
                        };
                    },
                    Action::Stop => {
                        break 'loop_label
                    },
                };
                i = i + 1;
            };
        };

        actions.destroy!(|_| {});

        assert!(total_moves == 100);
    }
}