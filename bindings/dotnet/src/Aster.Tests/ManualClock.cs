using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Aster.Tests;

/// <summary>
/// A <see cref="TimeProvider"/> that moves only when the test calls <see cref="Advance"/>: retry
/// back-offs, breaker recovery and deadline budgets are tested without real sleeps. Timers (what
/// <c>Task.Delay(delay, provider)</c> creates) fire from <see cref="Advance"/>;
/// <see cref="WaitForTimerAsync"/> waits until one has been armed, e.g. a call entering its back-off.
/// </summary>
internal sealed class ManualClock : TimeProvider
{
    private readonly object _gate = new();
    private readonly List<ManualTimer> _timers = [];
    private readonly SemaphoreSlim _armed = new(0);
    private DateTimeOffset _utcNow = new(2026, 1, 1, 0, 0, 0, TimeSpan.Zero);
    private long _ticks;

    public override DateTimeOffset GetUtcNow() { lock (_gate) return _utcNow; }

    public override long TimestampFrequency => TimeSpan.TicksPerSecond;

    public override long GetTimestamp() { lock (_gate) return _ticks; }

    public override ITimer CreateTimer(TimerCallback callback, object? state, TimeSpan dueTime, TimeSpan period)
    {
        var timer = new ManualTimer(this, callback, state);
        timer.Change(dueTime, period);
        return timer;
    }

    /// <summary>True once a timer has been armed since the last call; false if none was within the wait budget.</summary>
    public Task<bool> WaitForTimerAsync() => _armed.WaitAsync(WaitBudget.Value);

    /// <summary>Moves the clock forward and fires every timer that has come due.</summary>
    public void Advance(TimeSpan by)
    {
        List<ManualTimer> due;
        lock (_gate)
        {
            _utcNow += by;
            _ticks += by.Ticks;
            due = _timers.Where(t => t.DueAt <= _ticks).ToList();
            foreach (var t in due)
            {
                if (t.Period > TimeSpan.Zero && t.Period != Timeout.InfiniteTimeSpan) t.DueAt += t.Period.Ticks;
                else _timers.Remove(t);
            }
        }
        foreach (var t in due) t.Fire();
    }

    private sealed class ManualTimer(ManualClock clock, TimerCallback callback, object? state) : ITimer
    {
        public long DueAt;
        public TimeSpan Period;

        public void Fire() => callback(state);

        public bool Change(TimeSpan dueTime, TimeSpan period)
        {
            lock (clock._gate)
            {
                clock._timers.Remove(this);
                if (dueTime == Timeout.InfiniteTimeSpan) return true;
                DueAt = clock._ticks + dueTime.Ticks;
                Period = period;
                clock._timers.Add(this);
            }
            clock._armed.Release();
            return true;
        }

        public void Dispose() { lock (clock._gate) clock._timers.Remove(this); }

        public ValueTask DisposeAsync() { Dispose(); return ValueTask.CompletedTask; }
    }
}
