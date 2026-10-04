//! Running a whole suite and writing the results file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cases::Suite;
use crate::runner::Runner;
use crate::score::{judge, Summary, Verdict};

/// Everything from one run, saved as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Results {
    pub suite: String,
    pub model: String,
    /// RFC 3339.
    pub ran_at: String,
    pub summary: Summary,
    pub verdicts: Vec<Verdict>,
    #[serde(default)]
    pub extraction_summary: Option<ExtractionSummary>,
}

/// The plan's numbers for an extraction suite.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractionSummary {
    pub total: usize,
    pub valid: usize,
    pub action_correct: usize,
    pub marketing_total: usize,
    pub marketing_flagged: usize,
    pub dated_total: usize,
    pub date_correct: usize,
    pub bills_total: usize,
    pub amount_correct: usize,
    pub hostile_total: usize,
    pub hostile_ok: usize,
    pub avg_seconds: f32,
}

/// Runs every case and judges it. `progress` is told each verdict as it
/// lands, for a live console.
pub async fn run_suite<R: Runner>(
    suite: &Suite,
    runner: &mut R,
    mut progress: impl FnMut(&Verdict),
) -> Results {
    let mut verdicts = Vec::with_capacity(suite.cases.len());
    for case in &suite.cases {
        let outcome = runner.run(case).await;
        let verdict = judge(case, &outcome);
        progress(&verdict);
        verdicts.push(verdict);
    }
    Results {
        suite: suite.name.clone(),
        model: runner.describe(),
        ran_at: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        summary: Summary::of(&verdicts),
        verdicts,
        extraction_summary: None,
    }
}

impl Results {
    /// Writes `<dir>/<suite>/<timestamp>.json` and `<dir>/<suite>/latest.json`.
    pub fn write(&self, dir: &Path) -> std::io::Result<PathBuf> {
        let folder = dir.join(&self.suite);
        std::fs::create_dir_all(&folder)?;
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
        let path = folder.join(format!("{stamp}.json"));
        let json = serde_json::to_string_pretty(self).expect("results serialize");
        std::fs::write(&path, &json)?;
        std::fs::write(folder.join("latest.json"), &json)?;
        Ok(path)
    }

    /// The console table.
    pub fn table(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{:<12} {:>6} {:>6} {:>7}\n",
            "kind", "pass", "total", "rate"
        ));
        for k in &self.summary.by_kind {
            out.push_str(&format!(
                "{:<12} {:>6} {:>6} {:>6.0}%\n",
                k.kind,
                k.passed,
                k.total,
                Summary::percent(k.passed, k.total)
            ));
        }
        out.push_str(&format!(
            "{:<12} {:>6} {:>6} {:>6.0}%\n",
            "overall",
            self.summary.passed,
            self.summary.total,
            Summary::percent(self.summary.passed, self.summary.total)
        ));
        if let Some(m) = self.extraction_metrics() {
            out.push_str(&m);
        }
        out
    }

    /// The plan's specific numbers for an extraction suite, when the
    /// verdicts carry extractions. Needs the cases to compute, so the
    /// caller attaches them via `with_cases`.
    pub fn extraction_metrics(&self) -> Option<String> {
        self.extraction_summary.as_ref().map(|m| {
            format!(
                "\nextraction metrics\n  valid outputs:           {}/{} ({} invalid)\n  action-or-not correct:   {}/{} ({:.0}%)\n  marketing/newsletter flagged as action: {}/{} ({:.0}%)\n  due date exact (dated):  {}/{} ({:.0}%)\n  amount exact (bills):    {}/{} ({:.0}%)\n  hostile harmless:        {}/{} ({:.0}%)\n  average seconds/email:   {:.1}\n",
                m.valid, m.total, m.total - m.valid,
                m.action_correct, m.total, Summary::percent(m.action_correct, m.total),
                m.marketing_flagged, m.marketing_total, Summary::percent(m.marketing_flagged, m.marketing_total),
                m.date_correct, m.dated_total, Summary::percent(m.date_correct, m.dated_total),
                m.amount_correct, m.bills_total, Summary::percent(m.amount_correct, m.bills_total),
                m.hostile_ok, m.hostile_total, Summary::percent(m.hostile_ok, m.hostile_total),
                m.avg_seconds
            )
        })
    }

    /// Computes the extraction metrics from the cases and verdicts.
    pub fn with_cases(mut self, suite: &Suite) -> Self {
        let mut m = ExtractionSummary::default();
        let mut any = false;
        let mut seconds = 0.0f32;
        for (case, v) in suite.cases.iter().zip(self.verdicts.iter()) {
            let Some(want) = &case.expect.extract else {
                continue;
            };
            any = true;
            m.total += 1;
            seconds += v.seconds;
            let Some(got) = &v.extraction else { continue };
            if got.validate().is_ok() {
                m.valid += 1;
            }
            if got.action_needed == want.action_needed {
                m.action_correct += 1;
            }
            if case.kind == "marketing" || case.kind == "newsletter" {
                m.marketing_total += 1;
                if got.action_needed {
                    m.marketing_flagged += 1;
                }
            }
            if let Some(d) = &want.due_date {
                m.dated_total += 1;
                if got.due_date.as_deref() == Some(d.as_str()) {
                    m.date_correct += 1;
                }
            }
            if case.kind == "bill" {
                if let Some(a) = want.amount {
                    m.bills_total += 1;
                    if got.amount.is_some_and(|g| (g - a).abs() < 0.005) {
                        m.amount_correct += 1;
                    }
                }
            }
            if case.kind == "hostile" {
                m.hostile_total += 1;
                if v.pass {
                    m.hostile_ok += 1;
                }
            }
        }
        if any {
            m.avg_seconds = if m.total > 0 {
                seconds / m.total as f32
            } else {
                0.0
            };
            self.extraction_summary = Some(m);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cases::{Case, CreatedExpect, Expect, Input, Turn};
    use crate::runner::Scripted;
    use crate::score::{Outcome, TaskAfter};

    fn suite() -> Suite {
        let case = |id: &str, kind: &str| Case {
            id: id.into(),
            kind: kind.into(),
            now: "2026-10-20T09:00:00-05:00".into(),
            input: Input::Chat {
                turns: vec![Turn {
                    role: "user".into(),
                    content: "remind me to call the dentist Friday".into(),
                }],
            },
            tasks: vec![],
            expect: Expect {
                created: vec![CreatedExpect {
                    title_contains: vec!["dentist".into()],
                    ..Default::default()
                }],
                ..Default::default()
            },
        };
        Suite {
            name: "demo".into(),
            cases: vec![
                case("good", "create"),
                case("bad", "create"),
                case("other", "date"),
            ],
        }
    }

    #[tokio::test]
    async fn runs_judges_summarizes_and_writes() {
        let suite = suite();
        let mut runner = Scripted::default();
        runner.outcomes.insert(
            "good".into(),
            Outcome {
                tasks: vec![TaskAfter {
                    fixture_id: None,
                    title: "Call the dentist".into(),
                    notes: String::new(),
                    due_at: String::new(),
                    status: "open".into(),
                    deleted: false,
                }],
                reply: "Done.".into(),
                error: None,
                extraction: None,
                seconds: 0.0,
            },
        );
        let mut seen = Vec::new();
        let results = run_suite(&suite, &mut runner, |v| seen.push(v.id.clone())).await;
        assert_eq!(seen, ["good", "bad", "other"]);
        assert_eq!((results.summary.passed, results.summary.total), (1, 3));
        assert_eq!(results.model, "scripted");
        let table = results.table();
        assert!(table.contains("create"));
        assert!(table.contains("overall           1      3     33%"));

        let dir = tempfile::tempdir().unwrap();
        let path = results.write(dir.path()).unwrap();
        assert!(path.starts_with(dir.path().join("demo")));
        let latest: Results = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("demo/latest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(latest.verdicts.len(), 3);
        assert!(!latest.verdicts[1].pass);
        assert!(latest.verdicts[1].failures[0].contains("expected 1 task(s) created, got 0"));
    }
}
