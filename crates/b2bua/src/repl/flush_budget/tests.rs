//! A body holds its bytes of the budget until its last copy drops or
//! releases them; one larger than the budget holds all of it, an empty one
//! nothing, and a body that finds the budget spent waits for bytes to come
//! back.

use std::sync::Arc;
use std::time::Duration;

use super::{FlushBody, FlushBudget};

async fn held(budget: &FlushBudget, len: usize) -> FlushBody {
    let mut body = FlushBody::from(Arc::<[u8]>::from(vec![0; len]));
    budget.hold(&mut body).await;
    body
}

#[tokio::test]
async fn a_body_holds_its_bytes_until_its_last_copy_lets_them_go() {
    let budget = FlushBudget::new(100);
    let body = held(&budget, 30).await;
    assert!(body.holds());
    assert_eq!(budget.in_use(), 30);
    let mut copy = body.clone();
    drop(body);
    assert_eq!(budget.in_use(), 30, "a clone holds the same bytes");
    copy.release();
    assert_eq!(budget.in_use(), 0);
    assert_eq!(copy.len(), 30, "the body stays whole");
}

#[tokio::test]
async fn a_body_past_the_budget_holds_all_of_it_and_an_empty_one_nothing() {
    let budget = FlushBudget::new(100);
    let empty = held(&budget, 0).await;
    assert!(!empty.holds());
    let large = held(&budget, 250).await;
    assert_eq!(budget.in_use(), 100);
    drop(large);
    assert_eq!(budget.in_use(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_body_that_finds_the_budget_spent_waits_for_bytes_to_come_back() {
    let budget = FlushBudget::new(100);
    let first = held(&budget, 80).await;
    let waiting = tokio::spawn({
        let budget = budget.clone();
        async move { held(&budget, 40).await }
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!waiting.is_finished(), "40 bytes do not fit beside 80 of 100");
    drop(first);
    let second = waiting.await.unwrap();
    assert!(second.holds());
    assert_eq!(budget.in_use(), 40);
}
