# Payment Receive Design Improvements

## Problem Statement

The current `receive_payment()` function has a fundamental design flaw:

1. **Blocking**: It blocks for `min_threshold_ms + quiet_threshold_ms` (~15-20s) before returning
2. **Delayed Confirmation**: Even if a payment arrives in second 1, the customer doesn't see confirmation until the function returns
3. **Reliability vs UX tradeoff**: Longer timeouts improve reliability (channel opens succeed) but hurt UX (slow confirmations)

### Current Flow

```
Webhook arrives → node.receivePayments() BLOCKS 15+ seconds → Returns payments → Customer sees confirmation
                                    ↑
                   Payment arrives here (second 3) but customer waits until second 15+
```

---

## Solution 1: Disable Anchor Channels (Already Implemented)

**Status**: ✅ Implemented in `lightning-js/src/lib.rs` lines 271-279

This addresses reliability by ensuring LSP opens non-anchor channels directly, reducing the time needed for channel negotiation.

---

## Solution 2: Immediate Payment Callbacks

Two implementation options for immediate customer confirmation:

### Option A: Callback-Based (Simpler)

Pass a JavaScript callback to `receive_payment` that fires immediately when a payment arrives.

### Option B: Event Emitter Pattern (More Flexible)

Register event listeners before starting the receive loop, allowing multiple subscribers.

---

# Option A: Callback-Based Implementation

## Overview

Add a new `receive_payment_with_callback()` function that accepts a callback which fires immediately when payments are received, while still keeping the node alive for the full timeout period.

## Changes Required

### 1. lightning-js/src/lib.rs

#### Add new struct for callback data

```rust
#[napi(object)]
pub struct PaymentCallbackData {
  pub payment_hash: String,
  pub amount_msat: i64,
}
```

#### Add new method

```rust
#[napi]
pub fn receive_payment_with_callback(
  &self,
  env: Env,
  min_threshold_ms: i64,
  quiet_threshold_ms: i64,
  on_payment_received: JsFunction,
) -> napi::Result<Vec<ReceivedPayment>> {
  // Create threadsafe function for callback
  let tsfn: ThreadsafeFunction<PaymentCallbackData> = on_payment_received
    .create_threadsafe_function(0, |ctx| {
      let env = ctx.env;
      let data = ctx.value;
      let mut obj = env.create_object()?;
      obj.set_named_property("paymentHash", env.create_string(&data.payment_hash)?)?;
      obj.set_named_property("amountMsat", env.create_int64(data.amount_msat)?)?;
      Ok(vec![obj.into_unknown()])
    })?;

  let mut received_payments = vec![];

  if let Err(err) = self.node.start() {
    eprintln!("[lightning-js] Failed to start node: {err}");
    return Ok(received_payments);
  }

  if let Err(err) = self.node.sync_wallets() {
    eprintln!("[lightning-js] Failed to sync wallets: {err}");
    panic!("failed to sync wallets: {err}");
  }

  let start_sync_at = std::time::Instant::now();
  let mut last_event_time = start_sync_at;

  loop {
    let now = std::time::Instant::now();
    let total_time_elapsed = now.duration_since(start_sync_at).as_millis() as i64;
    let quiet_time_elapsed = now.duration_since(last_event_time).as_millis() as i64;

    if total_time_elapsed >= min_threshold_ms && quiet_time_elapsed >= quiet_threshold_ms {
      break;
    }

    if let Some(event) = self.node.next_event() {
      eprintln!("[lightning-js] Event: {event:?}");

      if let Event::PaymentReceived {
        payment_hash,
        amount_msat,
        ..
      } = &event
      {
        let payment_hash_hex = bytes_to_hex(&payment_hash.0);

        // Fire callback IMMEDIATELY
        let callback_data = PaymentCallbackData {
          payment_hash: payment_hash_hex.clone(),
          amount_msat: *amount_msat as i64,
        };
        tsfn.call(Ok(callback_data), ThreadsafeFunctionCallMode::Blocking);

        received_payments.push(ReceivedPayment {
          payment_hash: payment_hash.to_string(),
          amount: *amount_msat as i64,
        });
      }

      // Handle other events (PaymentClaimable, PaymentFailed, etc.) with logging
      // ...

      if let Err(err) = self.node.event_handled() {
        eprintln!("[lightning-js] Error while marking event handled: {err}");
      }
      last_event_time = now;
    }

    std::thread::sleep(std::time::Duration::from_millis(10));
  }

  if let Err(err) = self.node.stop() {
    eprintln!("[lightning-js] Failed to stop node: {err}");
  }

  Ok(received_payments)
}
```

### 2. mdk-checkout/packages/core/src/lightning-node.ts

#### Update MoneyDevKitNode class

```typescript
// Add new method alongside existing receivePayments()
receivePaymentsWithCallback(onPaymentReceived: (payment: { paymentHash: string; amountMsat: number }) => void) {
  return this.node.receivePaymentWithCallback(
    RECEIVE_PAYMENTS_MIN_THRESHOLD_MS,
    RECEIVE_PAYMENTS_QUIET_THRESHOLD_MS,
    (err: unknown, payment: { paymentHash: string; amountMsat: number }) => {
      if (err) {
        console.error('[MoneyDevKitNode] Payment callback error:', err);
        return;
      }
      onPaymentReceived(payment);
    }
  );
}
```

### 3. mdk-checkout/packages/core/src/handlers/webhooks.ts

#### Update webhook handler

```typescript
async function handleIncomingPayment() {
  const node = createMoneyDevKitNode();
  const client = createMoneyDevKitClient();

  // Payments are processed IMMEDIATELY as they arrive
  node.receivePaymentsWithCallback(async (payment) => {
    // Customer sees confirmation NOW
    markPaymentReceived(payment.paymentHash);

    try {
      await client.checkouts.paymentReceived({
        payments: [{
          paymentHash: payment.paymentHash,
          amountSats: payment.amountMsat / 1000,
          sandbox: false,
        }],
      });
    } catch (error) {
      warn("Failed to notify MoneyDevKit checkout", error);
    }
  });

  // Function still returns after timeouts for cleanup
}
```

## Pros

- Simple implementation using existing `ThreadsafeFunction` pattern (same as logging)
- Minimal changes to existing code structure
- Backward compatible (old `receive_payment` still works)
- Clear control flow

## Cons

- Slightly more verbose API
- Callback can't easily signal "stop early"

## Complexity

- **Rust changes**: ~50 lines of new code
- **TypeScript changes**: ~20 lines of new code
- **Risk**: Low - uses proven patterns already in codebase

---

# Option B: Event Emitter Pattern

## Overview

Create a stateful event listener system where callbacks are registered before starting the receive loop. Events fire to all registered listeners immediately.

## Changes Required

### 1. lightning-js/src/lib.rs

#### Add event listener storage to MdkNode

```rust
use std::sync::Mutex;

#[napi]
pub struct MdkNode {
  node: Node,
  network: Network,
  payment_listeners: Arc<Mutex<Vec<ThreadsafeFunction<PaymentCallbackData>>>>,
}

impl MdkNode {
  // ... existing constructor updated to initialize payment_listeners

  #[napi]
  pub fn on_payment_received(&self, env: Env, callback: JsFunction) -> napi::Result<()> {
    let tsfn: ThreadsafeFunction<PaymentCallbackData> = callback
      .create_threadsafe_function(0, |ctx| {
        let env = ctx.env;
        let data = ctx.value;
        let mut obj = env.create_object()?;
        obj.set_named_property("paymentHash", env.create_string(&data.payment_hash)?)?;
        obj.set_named_property("amountMsat", env.create_int64(data.amount_msat)?)?;
        Ok(vec![obj.into_unknown()])
      })?;

    let mut listeners = self.payment_listeners.lock().unwrap();
    listeners.push(tsfn);
    Ok(())
  }

  #[napi]
  pub fn remove_all_payment_listeners(&self) {
    let mut listeners = self.payment_listeners.lock().unwrap();
    listeners.clear();
  }

  // Helper to fire events to all listeners
  fn emit_payment_received(&self, payment_hash: String, amount_msat: i64) {
    let listeners = self.payment_listeners.lock().unwrap();
    let data = PaymentCallbackData { payment_hash, amount_msat };

    for tsfn in listeners.iter() {
      tsfn.call(Ok(data.clone()), ThreadsafeFunctionCallMode::NonBlocking);
    }
  }

  // Updated receive_payment that uses listeners
  #[napi]
  pub fn wait_for_events(
    &self,
    min_threshold_ms: i64,
    quiet_threshold_ms: i64,
  ) -> Vec<ReceivedPayment> {
    // ... same loop as receive_payment but calls emit_payment_received()
    // instead of just collecting payments
  }
}
```

### 2. mdk-checkout/packages/core/src/lightning-node.ts

#### Add event emitter wrapper

```typescript
export class MoneyDevKitNode {
  private node: LightningNodeInstance;
  private paymentHandlers: Set<(payment: ReceivedPayment) => void> = new Set();

  onPaymentReceived(handler: (payment: ReceivedPayment) => void): () => void {
    this.paymentHandlers.add(handler);

    // Register with native node
    this.node.onPaymentReceived((err: unknown, payment: ReceivedPayment) => {
      if (err) {
        console.error('[MoneyDevKitNode] Payment event error:', err);
        return;
      }
      handler(payment);
    });

    // Return unsubscribe function
    return () => {
      this.paymentHandlers.delete(handler);
    };
  }

  waitForEvents() {
    return this.node.waitForEvents(
      RECEIVE_PAYMENTS_MIN_THRESHOLD_MS,
      RECEIVE_PAYMENTS_QUIET_THRESHOLD_MS,
    );
  }
}
```

### 3. mdk-checkout/packages/core/src/handlers/webhooks.ts

#### Update webhook handler

```typescript
async function handleIncomingPayment() {
  const node = createMoneyDevKitNode();
  const client = createMoneyDevKitClient();

  // Register listener BEFORE waiting
  node.onPaymentReceived(async (payment) => {
    // Customer sees confirmation IMMEDIATELY
    markPaymentReceived(payment.paymentHash);

    try {
      await client.checkouts.paymentReceived({
        payments: [{
          paymentHash: payment.paymentHash,
          amountSats: payment.amount / 1000,
          sandbox: false,
        }],
      });
    } catch (error) {
      warn("Failed to notify MoneyDevKit checkout", error);
    }
  });

  // Start waiting for events (fires callbacks as they arrive)
  node.waitForEvents();
}
```

## Pros

- Familiar event emitter pattern for JS developers
- Multiple listeners supported
- Can add other event types easily (PaymentFailed, ChannelOpened, etc.)
- More extensible for future use cases

## Cons

- More complex implementation
- Requires managing listener lifecycle
- State management across start/stop cycles is tricky

## Complexity

- **Rust changes**: ~100 lines of new/modified code
- **TypeScript changes**: ~40 lines of new code
- **Risk**: Medium - new patterns, state management concerns

---

# Comparison

| Aspect | Option A (Callback) | Option B (Event Emitter) |
|--------|---------------------|-------------------------|
| **Complexity** | Low | Medium |
| **Lines of code** | ~70 | ~140 |
| **Learning curve** | Minimal | Low |
| **Extensibility** | Limited | High |
| **Multiple subscribers** | No | Yes |
| **Risk** | Low | Medium |
| **Time to implement** | 2-4 hours | 4-8 hours |

---

# Recommendation

**Start with Option A (Callback-Based)** because:

1. Uses the exact same `ThreadsafeFunction` pattern already working for logging
2. Solves the immediate problem with minimal changes
3. Lower risk for a staging-blocking issue
4. Can migrate to Option B later if extensibility is needed

## Implementation Order

1. ✅ Disable anchor channels (already done)
2. Implement Option A callback in `lightning-js`
3. Update `mdk-checkout` to use callback
4. Test in staging with reduced timeouts:
   - `min_threshold_ms`: 5000 (can be shorter now)
   - `quiet_threshold_ms`: 2000 (payment confirmed immediately anyway)
5. Measure customer confirmation latency improvement

## Expected Outcome

| Metric | Before | After (Option A) |
|--------|--------|------------------|
| **Payment confirmation** | 15-20s | <1s after payment |
| **Node alive time** | 15-20s | 5-7s |
| **Reliability** | ✅ Works | ✅ Works |
| **Customer UX** | ❌ Slow | ✅ Fast |

---

# Implementation Status & Learnings

## Option A: Implemented ✅

The callback-based approach was implemented as described above:
- `receive_payment_with_callback()` added to `lightning-js/src/lib.rs`
- `receivePaymentsWithCallback()` added to `mdk-checkout`
- Webhook handler updated to use callbacks

## Discovered Limitation: Node.js Event Loop Blocking

**The callbacks fire, but RPC execution is deferred until after Rust returns.**

### Root Cause

When Rust calls a JavaScript callback via `ThreadsafeFunction`:
1. Node.js is **single-threaded**
2. While Rust runs synchronously, the Node.js event loop is **blocked**
3. Callbacks return immediately (queuing Promises)
4. Queued Promises only execute **after Rust returns**

```
┌─────────────────────────────────────────────────────────────┐
│                    Node.js Event Loop                        │
│  ┌─────────────────────────────────────────────────────────┐│
│  │ Rust receive_payment_with_callback() [BLOCKING]         ││
│  │   ├── Start node                                        ││
│  │   ├── Event loop (15s)                                  ││
│  │   │     ├── PaymentReceived → callback fires            ││
│  │   │     │   └── RPC Promise QUEUED ← Can't execute!     ││
│  │   │     ├── PaymentReceived → callback fires            ││
│  │   │     │   └── RPC Promise QUEUED ← Can't execute!     ││
│  │   │     └── ... continues ...                           ││
│  │   └── Return                                            ││
│  └─────────────────────────────────────────────────────────┘│
│  ┌─────────────────────────────────────────────────────────┐│
│  │ NOW: Queued Promises finally execute                    ││
│  │   └── Customer sees confirmation (15s+ after payment)  ││
│  └─────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────┘
```

### Observed Behavior

From testing:
- Callbacks ARE firing (4 individual RPC calls observed)
- Each payment gets its own RPC (not batched) ✅
- But ALL RPC calls execute AFTER `receive_payment_with_callback()` returns ❌
- Customer waits full `min_threshold_ms` before seeing confirmation

### Current Workaround

Reduce timeouts to minimize wait time:
```typescript
const RECEIVE_PAYMENTS_MIN_THRESHOLD_MS = 5000   // Was 15000
const RECEIVE_PAYMENTS_QUIET_THRESHOLD_MS = 2000  // Was 5000
```

This reduces customer wait from ~15s to ~5s, but doesn't solve the fundamental issue.

---

# Solution 3: Async Rust with Promise Awaiting (Proper Fix)

## Overview

Make `receive_payment_with_callback` an **async function** that properly awaits JavaScript Promises, allowing Node.js event loop to run between events.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Node.js Event Loop                        │
│                                                              │
│  async receive_payment_with_callback()                       │
│    ├── Start node                                           │
│    ├── Loop:                                                │
│    │     ├── await next_event()     ← Node.js can run!     │
│    │     ├── PaymentReceived                                │
│    │     │   └── await callback()   ← RPC executes NOW!    │
│    │     │       └── Customer sees confirmation             │
│    │     └── continue loop...                               │
│    └── Return                                               │
└─────────────────────────────────────────────────────────────┘
```

## Changes Required

### 1. Cargo.toml - Enable async features

```toml
[dependencies]
napi = { version = "2", features = ["napi4", "async"] }
tokio = { version = "1", features = ["rt-multi-thread", "sync"] }
```

### 2. lightning-js/src/lib.rs - Async implementation

```rust
use napi::bindgen_prelude::*;
use tokio::sync::mpsc;

/// Async version that properly awaits JavaScript Promise callbacks
#[napi]
pub async fn receive_payment_with_callback_async(
  &self,
  min_threshold_ms: i64,
  quiet_threshold_ms: i64,
  on_payment_received: JsFunction,
) -> napi::Result<Vec<ReceivedPayment>> {
  // Create a channel for async event processing
  let (tx, mut rx) = mpsc::channel::<Event>(32);
  
  // Spawn event polling in background
  let node = self.node.clone();
  let poll_handle = tokio::spawn(async move {
    loop {
      if let Some(event) = node.next_event() {
        if tx.send(event).await.is_err() {
          break;
        }
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  });

  let mut received_payments = vec![];
  let start = Instant::now();
  let mut last_event = start;

  // Main event loop - async, allows Node.js to run
  loop {
    let now = Instant::now();
    let total = now.duration_since(start).as_millis() as i64;
    let quiet = now.duration_since(last_event).as_millis() as i64;

    if total >= min_threshold_ms && quiet >= quiet_threshold_ms {
      break;
    }

    // Non-blocking receive with timeout
    match tokio::time::timeout(
      Duration::from_millis(100),
      rx.recv()
    ).await {
      Ok(Some(event)) => {
        if let Event::PaymentReceived { payment_hash, amount_msat, .. } = &event {
          let payment = ReceivedPayment {
            payment_hash: bytes_to_hex(&payment_hash.0),
            amount: *amount_msat as i64,
          };

          // Call JS callback and AWAIT the Promise
          // This yields to Node.js event loop, allowing RPC to execute
          let promise: Promise<()> = on_payment_received.call(None, &[payment.clone()])?;
          promise.await?;  // ← Node.js runs here, RPC completes!

          received_payments.push(payment);
        }
        last_event = now;
      }
      _ => continue,
    }
  }

  poll_handle.abort();
  Ok(received_payments)
}
```

### 3. Alternative: Use call_async from ThreadsafeFunction

NAPI-RS 2.x supports `call_async` which returns a future:

```rust
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

#[napi]
pub async fn receive_payment_with_callback_async(
  &self,
  min_threshold_ms: i64,
  quiet_threshold_ms: i64,
  on_payment_received: JsFunction,
) -> napi::Result<Vec<ReceivedPayment>> {
  // Create threadsafe function that returns Promise<void>
  let tsfn: ThreadsafeFunction<ReceivedPayment, Promise<()>> = on_payment_received
    .create_threadsafe_function(0, |ctx| {
      // ... convert to JS object
    })?;

  // ... event loop ...

  if let Event::PaymentReceived { payment_hash, amount_msat, .. } = &event {
    let payment = ReceivedPayment { /* ... */ };
    
    // call_async returns a Future that resolves when JS Promise resolves
    tsfn.call_async(Ok(payment.clone())).await?;  // ← Awaits Promise!
    
    received_payments.push(payment);
  }

  // ...
}
```

### 4. mdk-checkout TypeScript changes

```typescript
// lightning-node.ts
async receivePaymentsWithCallback(
  onPaymentReceived: (payment: ReceivedPayment) => Promise<void>
): Promise<ReceivedPayment[]> {
  // Now the callback MUST return a Promise that resolves when done
  return await this.node.receivePaymentWithCallbackAsync(
    RECEIVE_PAYMENTS_MIN_THRESHOLD_MS,
    RECEIVE_PAYMENTS_QUIET_THRESHOLD_MS,
    async (payment: ReceivedPayment) => {
      await onPaymentReceived(payment);
    }
  );
}

// webhooks.ts
async function handleIncomingPayment() {
  const node = createMoneyDevKitNode();
  const client = createMoneyDevKitClient();

  await node.receivePaymentsWithCallback(async (payment) => {
    markPaymentReceived(payment.paymentHash);
    
    // This MUST complete before Rust continues
    await client.checkouts.paymentReceived({
      payments: [{
        paymentHash: payment.paymentHash,
        amountSats: payment.amount / 1000,
        sandbox: false,
      }],
    });
    // Customer sees confirmation NOW, not after timeout!
  });
}
```

## Complexity Assessment

| Aspect | Estimate |
|--------|----------|
| **Rust changes** | ~150 lines (new async function + refactoring) |
| **TypeScript changes** | ~30 lines (update to async/await) |
| **NAPI-RS learning curve** | Medium (async patterns) |
| **Testing complexity** | Medium (async timing, error handling) |
| **Risk** | Medium (new async patterns, but well-documented) |
| **Time to implement** | 1-2 days |

## Key Considerations

### 1. Error Handling
If the JS callback Promise rejects, Rust needs to handle it gracefully:
```rust
match tsfn.call_async(Ok(payment.clone())).await {
  Ok(_) => { /* Success */ }
  Err(e) => {
    eprintln!("[lightning-js] Callback failed: {e}");
    // Continue processing other events
  }
}
```

### 2. Callback Timeout
Add a timeout to prevent hanging on slow callbacks:
```rust
match tokio::time::timeout(
  Duration::from_secs(5),
  tsfn.call_async(Ok(payment.clone()))
).await {
  Ok(Ok(_)) => { /* Success */ }
  Ok(Err(e)) => { /* JS error */ }
  Err(_) => { /* Timeout */ }
}
```

### 3. Backpressure
If many payments arrive quickly, the callback might not keep up. Consider:
- Buffering payments and processing in batches
- Parallel callback execution with `tokio::spawn`
- Configurable concurrency limits

### 4. Graceful Shutdown
Ensure in-flight callbacks complete before returning:
```rust
// Wait for any pending callbacks before shutdown
while let Some(_) = pending_callbacks.next().await {}
```

## Expected Outcome

| Metric | Current (Option A) | After (Solution 3) |
|--------|-------------------|-------------------|
| **Payment confirmation** | 5-15s (after timeout) | <500ms (immediate) |
| **RPC timing** | Batched at end | During event loop |
| **Node.js event loop** | Blocked | Free to run |
| **Customer UX** | ❌ Still slow | ✅ Truly instant |

---

# Implementation Roadmap

1. **Phase 1: Workaround (Done)**
   - ✅ Implement Option A callback
   - ✅ Reduce timeouts (5s/2s)
   - Result: 15s → 5s confirmation time

2. **Phase 2: Proper Fix (Solution 3)**
   - [ ] Add `async` feature to napi in Cargo.toml
   - [ ] Implement `receive_payment_with_callback_async`
   - [ ] Update mdk-checkout to use async version
   - [ ] Test with realistic payment scenarios
   - Result: 5s → <500ms confirmation time

3. **Phase 3: Optimization (Future)**
   - [ ] Add parallel callback execution
   - [ ] Implement backpressure handling
   - [ ] Add metrics/tracing for callback latency
   - Result: Handle high-volume concurrent payments
