use crate::prelude::InterpolationRegistry;
use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;
use bevy_replicon::client::confirm_history::ConfirmHistory;
use bevy_utils::prelude::DebugName;
use lightyear_core::history_buffer::{HistoryBuffer, HistoryState};
use lightyear_core::interpolation::Interpolated;
use lightyear_core::prelude::Tick;
use lightyear_replication::checkpoint::ReplicationCheckpointMap;
#[allow(unused_imports)]
use tracing::{info, trace};

/// Stores a buffer of past component values received from the remote
#[derive(Component, Debug, Reflect)]
pub struct ConfirmedHistory<C> {
    history: HistoryBuffer<C>,
    /// True when the newest anchor was synthesized from an empty mutate tick.
    ///
    /// Empty mutate ticks confirm that a component did not change. While this stays true, newer
    /// empty mutate ticks can slide the same anchor forward instead of cloning the value again.
    newest_is_unchanged: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) enum ConfirmedHistorySample<C> {
    Pending,
    Removed,
    Present(C),
}

impl<C> Default for ConfirmedHistory<C> {
    fn default() -> Self {
        Self {
            history: HistoryBuffer::<C>::default(),
            newest_is_unchanged: false,
        }
    }
}

impl<C> PartialEq for ConfirmedHistory<C> {
    fn eq(&self, other: &Self) -> bool {
        self.history.eq(&other.history) && self.newest_is_unchanged == other.newest_is_unchanged
    }
}

impl<C> ConfirmedHistory<C> {
    pub(crate) fn len(&self) -> usize {
        self.history.len()
    }

    /// Get the n-th oldest tick in the buffer (starts from n = 0)
    pub fn get_nth_tick(&self, n: usize) -> Option<Tick> {
        self.history.get_nth(n).map(|(t, _)| *t)
    }

    fn get_nth_state(&self, n: usize) -> Option<(Tick, &HistoryState<C>)> {
        self.history.get_nth(n).map(|(t, state)| (*t, state))
    }

    /// The oldest value in the history, which is used as the start value for the interpolation
    pub fn start(&self) -> Option<(Tick, &C)> {
        self.get_nth(0)
    }

    /// The second oldest value in the history, which is used as the end value for the interpolation
    pub fn end(&self) -> Option<(Tick, &C)> {
        self.get_nth(1)
    }

    /// The most recent value in the history.
    pub fn newest(&self) -> Option<(Tick, &C)> {
        match self.history.most_recent() {
            None | Some((_, HistoryState::Removed)) => None,
            Some((t, HistoryState::Updated(v))) => Some((*t, v)),
        }
    }

    /// Get the n-th oldest `(tick, value)` in the buffer (starts from n = 0), skipping removals.
    /// `pub` (was `pub(crate)`) so downstream diagnostics can dump the full keyframe buffer — needed to
    /// see how the buffer transforms across the interpolation snap (battlemage interp-stale-keyframe).
    pub fn get_nth(&self, n: usize) -> Option<(Tick, &C)> {
        match self.history.get_nth(n) {
            None | Some((_, HistoryState::Removed)) => None,
            Some((t, HistoryState::Updated(v))) => Some((*t, v)),
        }
    }

    /// Push a new value in the history.
    ///
    /// Normally updates arrive in tick order (`lightyear_replication::receive`), but under a post-stall
    /// packet burst they can arrive **out of order**. The interpolation consumers (`sample`, the drain in
    /// `update_confirmed_history`) all assume a monotonic buffer, so we insert sorted + dedup-by-tick
    /// (`add_update_sorted`) to keep that invariant regardless of arrival order — otherwise a late update
    /// followed by `push_unchanged` could leave a stale value selected as the interpolation `start`,
    /// rendering the entity backward (a visible snap).
    pub fn push(&mut self, tick: Tick, value: C) {
        self.history.add_update_sorted(tick, value);
        self.newest_is_unchanged = false;
    }

    /// Push a removal in the history. Sorted + dedup'd, same rationale as [`Self::push`].
    pub(crate) fn push_remove(&mut self, tick: Tick) {
        self.history.add_remove_sorted(tick);
        self.newest_is_unchanged = false;
    }

    /// Pop the oldest value in the history
    pub fn pop(&mut self) -> Option<(Tick, C)> {
        let popped = match self.history.pop() {
            None | Some((_, HistoryState::Removed)) => None,
            Some((t, HistoryState::Updated(v))) => Some((t, v)),
        };
        if self.history.len() == 0 {
            self.newest_is_unchanged = false;
        }
        popped
    }
}

impl<C: Clone> ConfirmedHistory<C> {
    /// Mark the newest value as unchanged at `tick`.
    ///
    /// If the newest anchor was already synthesized from an empty mutate tick, only its tick is
    /// advanced. Otherwise a single unchanged anchor is appended by cloning the newest value.
    pub(crate) fn push_unchanged(&mut self, tick: Tick) -> Option<Tick> {
        let (newest_tick, newest_value) = self.newest()?;
        if tick <= newest_tick {
            return None;
        }

        if self.newest_is_unchanged {
            self.history.set_most_recent_tick(tick);
        } else {
            self.history.add_update(tick, newest_value.clone());
            self.newest_is_unchanged = true;
        }
        Some(newest_tick)
    }
}

impl<C: Component + Clone> ConfirmedHistory<C> {
    pub(crate) fn sample(
        &self,
        interpolation_tick: Tick,
        interpolation_overstep: f32,
        interpolation_registry: &InterpolationRegistry,
    ) -> ConfirmedHistorySample<C> {
        let Some(previous_index) = (0..self.len())
            .take_while(|i| {
                self.get_nth_tick(*i)
                    .is_some_and(|tick| tick <= interpolation_tick)
            })
            .last()
        else {
            return ConfirmedHistorySample::Pending;
        };

        let Some((start_tick, start_state)) = self.get_nth_state(previous_index) else {
            return ConfirmedHistorySample::Pending;
        };
        let HistoryState::Updated(start) = start_state else {
            return ConfirmedHistorySample::Removed;
        };

        let Some((end_tick, HistoryState::Updated(end))) = self.get_nth_state(previous_index + 1)
        else {
            return ConfirmedHistorySample::Present(start.clone());
        };

        if !interpolation_registry.has_interpolation_fn::<C>() {
            return ConfirmedHistorySample::Present(start.clone());
        }

        // Clamp rather than extrapolate beyond the newest confirmed value. This
        // makes late packets converge to the freshest server state instead of
        // overshooting when motion changes direction.
        let fraction = (((interpolation_tick - start_tick) as f32 + interpolation_overstep)
            / (end_tick - start_tick) as f32)
            .clamp(0.0, 1.0);
        trace!(
            ?start_tick,
            ?end_tick,
            ?interpolation_tick,
            ?interpolation_overstep,
            ?fraction,
            "Interpolate {:?}",
            DebugName::type_name::<C>()
        );
        trace!(
            target: "lightyear_debug::interpolation",
            kind = "interpolation_history_sample",
            component = ?DebugName::type_name::<C>(),
            interpolation_tick = interpolation_tick.0,
            start_tick = start_tick.0,
            end_tick = end_tick.0,
            interpolation_overstep,
            fraction,
            history_len = self.len(),
            "sampled interpolation history"
        );
        ConfirmedHistorySample::Present(interpolation_registry.interpolate(
            start.clone(),
            end.clone(),
            fraction,
        ))
    }

    pub fn interpolate(
        &self,
        interpolation_tick: Tick,
        interpolation_overstep: f32,
        interpolation_registry: &InterpolationRegistry,
    ) -> Option<C> {
        let (start_tick, start) = self.start()?;
        // It is possible that the interpolation tick lags behind the buffered
        // anchors, for example if two fresh updates arrive after a long gap:
        // X...H1...H2. In that case interpolation should not run yet.
        if interpolation_tick < start_tick {
            return None;
        }

        let (end_tick, end) = self.end()?;
        // Clamp rather than extrapolate beyond the newest confirmed value. This
        // makes late packets converge to the freshest server state instead of
        // overshooting when motion changes direction.
        let fraction = (((interpolation_tick - start_tick) as f32 + interpolation_overstep)
            / (end_tick - start_tick) as f32)
            .clamp(0.0, 1.0);
        trace!(
            ?start_tick,
            ?end_tick,
            ?interpolation_tick,
            ?interpolation_overstep,
            ?fraction,
            "Interpolate {:?}",
            DebugName::type_name::<C>()
        );
        trace!(
            target: "lightyear_debug::interpolation",
            kind = "interpolation_history_sample",
            component = ?DebugName::type_name::<C>(),
            interpolation_tick = interpolation_tick.0,
            start_tick = start_tick.0,
            end_tick = end_tick.0,
            interpolation_overstep,
            fraction,
            history_len = self.len(),
            "sampled interpolation history"
        );
        Some(interpolation_registry.interpolate(start.clone(), end.clone(), fraction))
    }
}

/// When `Interpolated` is added after component `C` was already replicated onto the entity,
/// seed `ConfirmedHistory<C>` from the current value so interpolation has an anchor immediately.
///
/// This is the branch-local equivalent of `main`'s `#1421` fix, adapted to the current
/// Replicon marker-fn receive path. Component updates for interpolated entities are normally
/// captured by `registry::write_history::<C>`, but that only runs on future network updates.
/// If `Interpolated` arrives after `C`, we need to synthesize the initial history entry from the
/// existing component value and the entity's latest confirmed Replicon tick.
pub(crate) fn insert_confirmed_history_on_interpolated<C: Component + Clone>(
    trigger: On<Add, Interpolated>,
    mut commands: Commands,
    checkpoints: Res<ReplicationCheckpointMap>,
    query: Query<(&C, &ConfirmHistory), Without<ConfirmedHistory<C>>>,
) {
    let Ok((component, confirm_history)) = query.get(trigger.entity) else {
        return;
    };

    let Some(tick) = checkpoints.get(confirm_history.last_tick()) else {
        debug_assert!(
            false,
            "missing authoritative checkpoint mapping while backfilling ConfirmedHistory"
        );
        return;
    };

    let mut history = ConfirmedHistory::<C>::default();
    history.push(tick, component.clone());
    commands
        .entity(trigger.entity)
        .try_insert(history)
        .try_remove::<C>();
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::InterpolationRegistry;
    use bevy_app::App;
    use bevy_ecs::component::Component;
    use bevy_replicon::prelude::RepliconTick;

    #[derive(Component, Clone, Debug, PartialEq)]
    struct TestComp(f32);

    fn lerp(start: TestComp, end: TestComp, t: f32) -> TestComp {
        TestComp(start.0 + (end.0 - start.0) * t)
    }

    fn registry() -> InterpolationRegistry {
        let mut registry = InterpolationRegistry::default();
        registry.set_interpolation::<TestComp>(lerp);
        registry
    }

    /// Regression: a post-stall packet burst can deliver confirmed updates **out of tick order**. The
    /// buffer (and every consumer: `sample`, the drain) assumes monotonic-by-tick order, so out-of-order
    /// pushes must still leave the buffer sorted + deduplicated — otherwise `sample` selects a stale
    /// keyframe as the interpolation `start` and the entity renders backward (a visible snap).
    ///
    /// Models the observed failure (value == tick for constant-velocity motion): the real update for tick
    /// 1165 arrives, then late packets for 1159 and 1152. Before the fix the buffer was
    /// `[1146,1165,1159,1152]` and `sample(1165)` returned the stale 1152 value; after, it stays
    /// `[1146,1152,1159,1165]` and samples the real 1165 value.
    #[test]
    fn out_of_order_pushes_keep_history_sorted_and_sample_correct() {
        let mut history = ConfirmedHistory::<TestComp>::default();
        history.push(Tick(1146), TestComp(1146.0));
        history.push(Tick(1165), TestComp(1165.0)); // real update for tick 1165
        history.push(Tick(1159), TestComp(1159.0)); // late packet, out of order
        history.push(Tick(1152), TestComp(1152.0)); // late packet, out of order

        // The buffer must be sorted by tick with no duplicates, regardless of arrival order.
        let nth = |h: &ConfirmedHistory<TestComp>, n| h.get_nth(n).map(|(t, _)| t.0 as i64);
        assert_eq!(nth(&history, 0), Some(1146));
        assert_eq!(nth(&history, 1), Some(1152));
        assert_eq!(nth(&history, 2), Some(1159));
        assert_eq!(nth(&history, 3), Some(1165));
        assert_eq!(nth(&history, 4), None);

        // A duplicate tick from a late update must replace in place (newest value wins), not pile up.
        history.push(Tick(1159), TestComp(1159.5));
        assert_eq!(
            history.get_nth(2).map(|(t, v)| (t.0 as i64, v.0)),
            Some((1159, 1159.5))
        );
        let len = (0..).take_while(|n| history.get_nth(*n).is_some()).count();
        assert_eq!(len, 4, "duplicate tick must dedup, not grow the buffer");

        // Sampling at tick 1165 must return the REAL value (1165.0), not the stale 1152 value.
        let registry = registry();
        match history.sample(Tick(1165), 0.0, &registry) {
            ConfirmedHistorySample::Present(v) => assert_eq!(v, TestComp(1165.0)),
            other => panic!("expected Present(1165.0), got {other:?}"),
        }
    }

    #[test]
    fn interpolate_clamps_to_newest_value_when_tick_is_past_end() {
        let mut history = ConfirmedHistory::<TestComp>::default();
        history.push(Tick(10), TestComp(0.0));
        history.push(Tick(20), TestComp(10.0));

        let registry = registry();
        assert_eq!(
            history.interpolate(Tick(30), 0.0, &registry),
            Some(TestComp(10.0))
        );
        assert_eq!(
            history.interpolate(Tick(20), 0.5, &registry),
            Some(TestComp(10.0))
        );
    }

    #[test]
    fn interpolate_returns_none_with_single_keyframe() {
        let mut history = ConfirmedHistory::<TestComp>::default();
        history.push(Tick(10), TestComp(42.0));

        let registry = registry();
        assert_eq!(history.interpolate(Tick(10), 0.0, &registry), None);
        assert_eq!(history.interpolate(Tick(50), 0.5, &registry), None);
    }

    #[test]
    fn inserts_history_when_interpolated_added_after_component_is_already_replicated() {
        let mut app = App::new();
        app.insert_resource(ReplicationCheckpointMap::default());
        app.add_observer(insert_confirmed_history_on_interpolated::<TestComp>);

        let replicon_tick = RepliconTick::new(11);
        app.world_mut()
            .resource_mut::<ReplicationCheckpointMap>()
            .record(replicon_tick, Tick(42));

        let entity = app
            .world_mut()
            .spawn((TestComp(2.0), ConfirmHistory::new(replicon_tick)))
            .id();
        app.update();
        app.world_mut().entity_mut(entity).insert(Interpolated);
        app.update();

        let history = app
            .world()
            .entity(entity)
            .get::<ConfirmedHistory<TestComp>>()
            .unwrap();
        assert_eq!(
            history.start().map(|(tick, value)| (tick, value.clone())),
            Some((Tick(42), TestComp(2.0)))
        );
        assert!(
            !app.world().entity(entity).contains::<TestComp>(),
            "live interpolated component should be removed until the interpolation timeline reaches the history start tick"
        );
    }
}
