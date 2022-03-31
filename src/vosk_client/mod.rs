// Copyright (C) 2022 Rafael Caricio <rafael@caricio.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use serde_derive::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug)]
pub struct Configuration {
    config: ConfigInner,
}

#[derive(Deserialize, Serialize, Debug)]
struct ConfigInner {
    /// Sample rate the audio will be provided at.
    sample_rate: i32,

    /// Show time ranges of each word in the transcription.
    words: bool,
}

impl Configuration {
    pub fn new(sample_rate: i32) -> Self {
        Self {
            config: ConfigInner {
                sample_rate,
                // We always want to receive the words with their time ranges.
                words: true,
            },
        }
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub struct Transcript {
    pub result: Vec<WordInfo>,
    pub text: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct WordInfo {
    #[serde(rename = "conf")]
    pub confidence: f64,
    pub word: String,
    pub start: f64,
    pub end: f64,
}
