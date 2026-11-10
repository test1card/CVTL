//! Two-branch allocation and slew limits. Branch failure commands both branches off.
use cvtl_core::units::{Seconds, Watts, WattsPerSecond};

#[derive(Clone, Debug)]
pub struct Allocator {
    coarse: Option<usize>,
    total_up: f64,
    total_down: f64,
    branch_up: [f64; 2],
    branch_down: [f64; 2],
    previous: [f64; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocationError;

#[derive(Clone, Debug, PartialEq)]
pub struct Allocation {
    pub requested: Watts,
    /// Sum of the final branch commands, after all limits.
    pub limited: Watts,
    pub targets: [Watts; 2],
    pub branch_loss: bool,
    pub hard_limit: bool,
    pub slew_limited: bool,
}

impl Allocator {
    pub fn new(
        coarse: Option<usize>,
        total_up: WattsPerSecond,
        total_down: WattsPerSecond,
        branch_up: [WattsPerSecond; 2],
        branch_down: [WattsPerSecond; 2],
    ) -> Result<Self, AllocationError> {
        if coarse.is_some_and(|i| i >= 2)
            || [total_up, total_down]
                .iter()
                .chain(branch_up.iter())
                .chain(branch_down.iter())
                .any(|r| r.get() < 0.0)
        {
            return Err(AllocationError);
        }
        Ok(Self {
            coarse,
            total_up: total_up.get(),
            total_down: total_down.get(),
            branch_up: branch_up.map(|r| r.get()),
            branch_down: branch_down.map(|r| r.get()),
            previous: [0.0; 2],
        })
    }

    pub fn reset(&mut self) {
        self.previous = [0.0; 2];
    }

    pub fn step(
        &mut self,
        request: Watts,
        caps: [Watts; 2],
        total_cap: Watts,
        healthy: [bool; 2],
        dt: Seconds,
    ) -> Result<Allocation, AllocationError> {
        if dt.get() <= 0.0 || total_cap.get() < 0.0 || caps.iter().any(|x| x.get() < 0.0) {
            return Err(AllocationError);
        }
        let caps = caps.map(|x| x.get());
        let total_cap = total_cap.get();
        let previous = self.previous;
        let loss = !healthy.iter().all(|x| *x);
        let old_sum = previous[0] + previous[1];
        let max_branch = caps[0].max(caps[1]);
        // Ratio formulation avoids overflow when two large capacities are added.
        let cap_sum = if max_branch == 0.0 {
            0.0
        } else {
            (caps[0] / max_branch + caps[1] / max_branch) * max_branch
        };
        let allowed = total_cap.min(cap_sum);
        let demand = request.get().max(0.0).min(allowed);
        let mut desired = if max_branch == 0.0 {
            [0.0; 2]
        } else {
            let denom = caps[0] / max_branch + caps[1] / max_branch;
            [
                demand * (caps[0] / max_branch / denom),
                demand * (caps[1] / max_branch / denom),
            ]
        };
        if let Some(i) = self.coarse {
            if demand < old_sum {
                let j = 1 - i;
                desired[j] = previous[j].min(caps[j]).min(demand);
                desired[i] = (demand - desired[j]).min(caps[i]);
                // A newly reduced coarse cap may require use of the other branch.
                desired[j] = (demand - desired[i]).min(caps[j]);
            }
        }
        let hard_limit = loss
            || request.get() > allowed
            || previous.iter().zip(caps).any(|(p, c)| *p > c)
            || old_sum > total_cap;
        let mut out = [0.0; 2];
        if !loss {
            for i in 0..2 {
                out[i] = desired[i]
                    .max((previous[i] - self.branch_down[i] * dt.get()).max(0.0))
                    .min(previous[i] + self.branch_up[i] * dt.get())
                    .min(caps[i]);
            }
            // Enforce the aggregate rate by interpolating with the previous commands.
            // The intersection with hard limits below takes precedence over slew.
            let sum = out[0] + out[1];
            let upper = old_sum + self.total_up * dt.get();
            let lower = (old_sum - self.total_down * dt.get()).max(0.0);
            if sum > upper && sum > old_sum {
                let factor = ((upper - old_sum) / (sum - old_sum)).clamp(0.0, 1.0);
                for i in 0..2 {
                    out[i] = previous[i] + (out[i] - previous[i]) * factor;
                }
            } else if sum < lower && sum < old_sum {
                let factor = ((old_sum - lower) / (old_sum - sum)).clamp(0.0, 1.0);
                for i in 0..2 {
                    out[i] = previous[i] + (out[i] - previous[i]) * factor;
                }
            }
            for i in 0..2 {
                out[i] = out[i].min(caps[i]);
            }
            let sum = out[0] + out[1];
            if !sum.is_finite() {
                return Err(AllocationError);
            }
            if sum > total_cap {
                let factor = total_cap / sum;
                out = out.map(|v| v * factor);
                if out[0] + out[1] > total_cap {
                    out[1] = out[1].min((total_cap - out[0]).max(0.0));
                    if out[0] + out[1] > total_cap {
                        out[1] = out[1].next_down().max(0.0);
                    }
                }
            }
        }
        let sum = out[0] + out[1];
        if !sum.is_finite() || sum > total_cap || out.iter().any(|v| !v.is_finite()) {
            return Err(AllocationError);
        }
        self.previous = out;
        Ok(Allocation {
            requested: request,
            limited: Watts::new(sum).ok_or(AllocationError)?,
            targets: out.map(|v| Watts::new(v).expect("finite allocation")),
            branch_loss: loss,
            hard_limit,
            slew_limited: !loss && (sum - demand).abs() > 1e-12,
        })
    }
}
