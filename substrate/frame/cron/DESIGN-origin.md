# Scheduled origin variant — sketch for comparison

Reference only, not wired into the build. Compares the POC's current
`Signed(scheduler)` dispatch against Pablo's "general origin that pays for execution".

## Current POC (Signed origin)

`do_execute` dispatches the stored call as the scheduler:

```rust
let result = if T::ScheduleFilter::contains(&call) {
    call.dispatch(RawOrigin::Signed(task.scheduler.clone()).into())
        .map(|_| ())
        .map_err(|e| e.error)
} else {
    Err(frame_system::Error::<T>::CallFiltered.into())
};
```

Any dispatchable that takes a signed origin is schedulable, unchanged. The target
pallet cannot tell the call was scheduled.

## Variant (dedicated `Scheduled` origin)

### 1. Origin type (in the pallet module)

```rust
#[derive(
    PartialEq, Eq, Clone, Debug, Encode, Decode, DecodeWithMemTracking, TypeInfo, MaxEncodedLen,
)]
#[codec(mel_bound(AccountId: MaxEncodedLen))]
pub enum RawOrigin<AccountId> {
    /// Dispatched by the cron pallet on behalf of `who`, execution prepaid.
    Scheduled(AccountId),
}

#[pallet::origin]
pub type Origin<T> = RawOrigin<<T as frame_system::Config>::AccountId>;
```

`construct_runtime!` aggregates this into `RuntimeOrigin` automatically; `Cron: pallet_cron`
in the runtime needs no change.

### 2. Config bound

The pallet builds the origin, so `RuntimeOrigin` must be constructible from it:

```rust
pub trait Config:
    CreateBare<frame_system::Call<Self>> + pallet_timestamp::Config + frame_system::Config
{
    // ...existing items...

    // added:
    // RuntimeOrigin already ties to frame_system via the RuntimeCall bound; this adds the
    // From impl that construct_runtime! generates in the runtime.
}
// where <Self as frame_system::Config>::RuntimeOrigin: From<Origin<Self>>
```

### 3. Dispatch change in `do_execute`

```rust
let origin = Origin::<T>::Scheduled(task.scheduler.clone());
let result = if T::ScheduleFilter::contains(&call) {
    call.dispatch(origin.into()).map(|_| ()).map_err(|e| e.error)
} else {
    Err(frame_system::Error::<T>::CallFiltered.into())
};
```

A custom origin does not carry `frame_system::BaseCallFilter`, so the explicit
`ScheduleFilter` check becomes the sole filter guard. It is already present, so this is
no regression, but it is now load-bearing.

### 4. `EnsureOrigin` for consumers (shape follows pallet-collective's `EnsureMember`)

```rust
pub struct EnsureScheduled<T>(core::marker::PhantomData<T>);

impl<T: Config> EnsureOrigin<<T as frame_system::Config>::RuntimeOrigin> for EnsureScheduled<T>
where
    for<'a> &'a <<T as frame_system::Config>::RuntimeOrigin as OriginTrait>::PalletsOrigin:
        TryInto<&'a RawOrigin<T::AccountId>>,
{
    type Success = T::AccountId;
    fn try_origin(
        o: <T as frame_system::Config>::RuntimeOrigin,
    ) -> Result<Self::Success, <T as frame_system::Config>::RuntimeOrigin> {
        match o.caller().try_into() {
            Ok(RawOrigin::Scheduled(who)) => Ok(who.clone()),
            _ => Err(o),
        }
    }
}
```

A target pallet that wants to accept scheduled calls sets some
`type Origin: EnsureOrigin<...>` to `EnsureScheduled<Runtime>`.

## The catch

A call written as `ensure_signed(origin)?` — most of the runtime — REJECTS a `Scheduled`
origin. So a pure `Scheduled` origin makes only opt-in pallets schedulable. To keep broad
compatibility you would add an "or signed" escape hatch, but existing pallets hardcode
`ensure_signed`, so they cannot be retrofitted without their own change.

```rust
// Accepts either; still lets an aware pallet distinguish the two.
pub struct EnsureScheduledOrSigned<T>(core::marker::PhantomData<T>);
// try_origin: Ok on RawOrigin::Scheduled(who) or frame_system::RawOrigin::Signed(who)
```

## Comparison

| | `Signed(scheduler)` (POC) | `Scheduled(who)` origin |
|---|---|---|
| Schedulable calls | any signed-origin call, no target change | only pallets that opt into `EnsureScheduled` |
| Distinguishable as scheduled | no | yes |
| Filtering | `ScheduleFilter` + normal signed `BaseCallFilter` | `ScheduleFilter` only (custom origin bypasses base filter) |
| Target-pallet changes | none | each consumer wires `EnsureScheduled` |
| Calls needing a non-signed origin | not schedulable | schedulable if origin maps in |
| New surface | none | origin type, Config bound, `EnsureOrigin`, benchmark `try_successful_origin` |

## Recommendation

Keep `Signed(scheduler)` for the POC: it makes the whole runtime schedulable with zero
target changes, which is what contracts scheduling their own calls need. Add the
`Scheduled` origin only if a concrete target pallet must special-case scheduled execution.
The two can coexist: default to `Signed`, add the origin variant behind a config choice
when a use case appears.
