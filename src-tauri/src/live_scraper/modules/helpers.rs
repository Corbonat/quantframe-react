use std::{collections::{HashMap, HashSet}, path::Path, sync::OnceLock};

use entity::{dto::SubType as EntitySubType, stock_item::*, wish_list::*};
use serde_json::json;
use service::*;
use utils::*;
use wf_market::{
    enums::OrderType,
    types::{CreateOrderParams, OrderList, OrderWithUser, UpdateOrderParams},
};

use crate::{
    DATABASE,
    app::{Settings, StockItemSettings},
    cache::types::{CacheTradableItem, ItemPriceInfo},
    enums::*,
    live_scraper::*,
    send_event,
    types::*,
    utils::{
        ErrorFromExt, OrderExt, OrderListExt, SubTypeExt, modules::states, order_ext::OrderDetails,
    },
};

pub static INTERESTING_ITEMS: OnceLock<HashMap<String, Vec<ItemPriceInfo>>> = OnceLock::new();
const MOD_TAG: &str = "mod";
const MODS_TAG: &str = "mods";

pub fn is_disabled(value: i64) -> bool {
    value <= -1
}

fn weighted_average(values: impl Iterator<Item = (f64, f64)>) -> f64 {
    let mut weighted_sum = 0.0;
    let mut total_weight = 0.0;

    for (value, weight) in values {
        if !value.is_finite() {
            continue;
        }
        let normalized_weight = if weight.is_finite() && weight > 0.0 {
            weight
        } else {
            1.0
        };
        weighted_sum += value * normalized_weight;
        total_weight += normalized_weight;
    }

    if total_weight <= 0.0 {
        0.0
    } else {
        weighted_sum / total_weight
    }
}

fn min_rank_sub_type(items: &[ItemPriceInfo]) -> Option<EntitySubType> {
    let mut min_rank_sub_type = None;
    let mut min_rank = i64::MAX;

    for item in items {
        if let Some(sub_type) = &item.sub_type {
            if let Some(rank) = sub_type.rank {
                if rank < min_rank {
                    min_rank = rank;
                    min_rank_sub_type = Some(sub_type.clone());
                }
            } else if min_rank_sub_type.is_none() {
                min_rank_sub_type = Some(sub_type.clone());
            }
        }
    }

    min_rank_sub_type
}

fn build_uuid(wfm_url: &str, sub_type: &Option<EntitySubType>) -> String {
    let mut uuid = wfm_url.to_string();
    if let Some(sub_type) = sub_type {
        let display = sub_type.shot_display();
        if !display.is_empty() {
            uuid.push_str(&format!("-{}", display));
        }
    }
    uuid
}

fn aggregate_rankless_item_price(entries: &[ItemPriceInfo]) -> Option<ItemPriceInfo> {
    if entries.is_empty() {
        return None;
    }
    let base = entries[0].clone();
    let volume = entries
        .iter()
        .map(|entry| if entry.volume.is_finite() { entry.volume.max(0.0) } else { 0.0 })
        .sum::<f64>();
    let min_price = entries
        .iter()
        .map(|entry| entry.min_price)
        .fold(f64::INFINITY, f64::min);
    let max_price = entries
        .iter()
        .map(|entry| entry.max_price)
        .fold(f64::NEG_INFINITY, f64::max);
    let avg_price = weighted_average(entries.iter().map(|entry| (entry.avg_price, entry.volume)));
    let moving_avg = {
        let values = entries
            .iter()
            .filter_map(|entry| entry.moving_avg.map(|moving| (moving, entry.volume)));
        let average = weighted_average(values);
        if average == 0.0 && avg_price > 0.0 {
            Some(avg_price)
        } else if average == 0.0 {
            None
        } else {
            Some(average)
        }
    };
    let median = weighted_average(entries.iter().map(|entry| (entry.median, entry.volume)));
    let profit = weighted_average(entries.iter().map(|entry| (entry.profit, entry.volume)));
    let profit_margin =
        weighted_average(entries.iter().map(|entry| (entry.profit_margin, entry.volume)));
    let week_price_shift =
        weighted_average(entries.iter().map(|entry| (entry.week_price_shift, entry.volume)));
    let sub_type = min_rank_sub_type(entries);

    Some(ItemPriceInfo {
        wfm_url: base.wfm_url.clone(),
        wfm_id: base.wfm_id.clone(),
        uuid: build_uuid(&base.wfm_url, &sub_type),
        volume,
        max_price: if max_price.is_finite() { max_price } else { 0.0 },
        min_price: if min_price.is_finite() { min_price } else { 0.0 },
        avg_price,
        moving_avg,
        median,
        profit,
        profit_margin,
        trading_tax: base.trading_tax,
        week_price_shift,
        sub_type,
    })
}

fn max_rank(item: &CacheTradableItem) -> Option<i64> {
    item.max_rank
        .or_else(|| item.sub_type.as_ref().and_then(|sub_type| sub_type.max_rank))
}

fn should_ignore_rank_for_low_rank_mod_with_flag(
    low_rank_mods_rankless_mode: bool,
    item: &CacheTradableItem,
) -> bool {
    if !low_rank_mods_rankless_mode {
        return false;
    }
    let is_mod = item
        .tags
        .iter()
        .any(|tag| tag.eq_ignore_ascii_case(MOD_TAG) || tag.eq_ignore_ascii_case(MODS_TAG));
    if !is_mod {
        return false;
    }
    matches!(max_rank(item), Some(rank) if rank > 0 && rank < 10)
}

pub fn should_ignore_rank_for_low_rank_mod(
    settings: &Settings,
    item: &CacheTradableItem,
) -> bool {
    should_ignore_rank_for_low_rank_mod_with_flag(
        settings.live_scraper.low_rank_mods_rankless_mode,
        item,
    )
}

fn low_rank_mod_ids(low_rank_mods_rankless_mode: bool) -> HashSet<String> {
    if !low_rank_mods_rankless_mode {
        return HashSet::new();
    }
    let cache = match states::cache_client() {
        Ok(cache) => cache,
        Err(_) => return HashSet::new(),
    };

    cache
        .tradable_item()
        .get_items()
        .unwrap_or_default()
        .into_iter()
        .filter(|item| should_ignore_rank_for_low_rank_mod_with_flag(true, item))
        .map(|item| item.wfm_id)
        .collect()
}

pub fn get_effective_item_price(
    item_info: &CacheTradableItem,
    sub_type: Option<EntitySubType>,
    settings: &Settings,
) -> Result<ItemPriceInfo, Error> {
    let cache = states::cache_client()?;
    if !should_ignore_rank_for_low_rank_mod(settings, item_info) {
        return Ok(cache
            .item_price()
            .find_by(&item_info.wfm_url_name, sub_type)?
            .unwrap_or_default());
    }
    let items = cache
        .item_price()
        .get_by_filter(|item| item.wfm_id == item_info.wfm_id);
    Ok(aggregate_rankless_item_price(&items).unwrap_or_default())
}

pub fn filter_market_orders_by_mode(
    live_orders: &mut OrderList<OrderWithUser>,
    sub_type: Option<EntitySubType>,
    item_info: &CacheTradableItem,
    settings: &Settings,
) {
    if should_ignore_rank_for_low_rank_mod(settings, item_info) {
        return;
    }
    live_orders.filter_by_sub_type(wf_market::types::SubType::from_entity(sub_type), false);
}

pub fn get_interesting_items(
    settings: &StockItemSettings,
    low_rank_mods_rankless_mode: bool,
) -> Vec<ItemPriceInfo> {
    let query_id = format!(
        "{};low_rank_mods_rankless_mode:{}",
        settings.get_query_id(),
        low_rank_mods_rankless_mode
    );
    if let Some(items) = INTERESTING_ITEMS.get() {
        if let Some(interesting_items) = items.get(&query_id) {
            return interesting_items.clone();
        }
    }
    let cache = states::cache_client().expect("Failed to get cache client");

    let volume_threshold = settings.volume_threshold;
    let avg_price_cap = settings.avg_price_cap;
    let trading_tax_cap = settings.trading_tax_cap;
    let profit = settings.profit_threshold;
    let profit_margin = settings.min_wtb_profit_margin;
    let price_shift_threshold = settings.price_shift_threshold;

    // Dynamic filter using closures

    let profit_margin_filter = |item: &ItemPriceInfo| {
        is_disabled(profit_margin) || item.profit_margin >= profit_margin as f64
    };

    let volume_filter = |item: &ItemPriceInfo| {
        is_disabled(volume_threshold) || item.volume > volume_threshold as f64
    };

    let profit_filter = |item: &ItemPriceInfo| is_disabled(profit) || item.profit > profit as f64;

    let avg_price_filter =
        |item: &ItemPriceInfo| is_disabled(avg_price_cap) || item.avg_price <= avg_price_cap as f64;

    let week_price_shift_filter = |item: &ItemPriceInfo| {
        is_disabled(price_shift_threshold) || item.week_price_shift >= price_shift_threshold as f64
    };

    let trading_tax_cap_filter =
        |item: &ItemPriceInfo| is_disabled(trading_tax_cap) || item.trading_tax < trading_tax_cap;

    // Combine multiple filters dynamically
    let combined_filter = |item: &ItemPriceInfo| {
        volume_filter(item)
            && profit_filter(item)
            && avg_price_filter(item)
            && week_price_shift_filter(item)
            && trading_tax_cap_filter(item)
            && profit_margin_filter(item)
    };

    let base_items = if low_rank_mods_rankless_mode {
        let low_rank_ids = low_rank_mod_ids(low_rank_mods_rankless_mode);
        let mut aggregated_by_item: HashMap<String, Vec<ItemPriceInfo>> = HashMap::new();
        let mut passthrough_items = Vec::new();

        for item in cache.item_price().get_items().unwrap_or_default() {
            if low_rank_ids.contains(&item.wfm_id) {
                aggregated_by_item
                    .entry(item.wfm_id.clone())
                    .or_default()
                    .push(item);
            } else {
                passthrough_items.push(item);
            }
        }

        for grouped in aggregated_by_item.into_values() {
            if let Some(aggregated) = aggregate_rankless_item_price(&grouped) {
                passthrough_items.push(aggregated);
            }
        }
        passthrough_items
    } else {
        cache.item_price().get_items().unwrap_or_default()
    };

    let items = base_items
        .into_iter()
        .filter(|item| combined_filter(item))
        .collect::<Vec<_>>();
    if items.is_empty() {
        info(
            "LiveScraper:Helpers:GetInterestingItems",
            &format!(
                "No interesting items found for settings: {}",
                query_id
            ),
            &LoggerOptions::default(),
        );
        return vec![];
    }
    items
}

pub fn knapsack(
    items: Vec<(i64, f64, String, String)>,
    max_weight: i64,
) -> (
    Vec<(i64, f64, String, String)>,
    Vec<(i64, f64, String, String)>,
) {
    let n = items.len();
    let w_max = max_weight as usize;

    // dp[w] = best value achievable with capacity w
    let mut dp = vec![0.0; w_max + 1];

    // choice[i][w] = true if item i is chosen when capacity is w
    let mut choice = vec![vec![false; w_max + 1]; n];

    for (i, item) in items.iter().enumerate() {
        let weight = item.0 as usize;
        let value = item.1;

        // iterate backwards for 1D DP
        for w in (weight..=w_max).rev() {
            let new_val = dp[w - weight] + value;
            if new_val > dp[w] {
                dp[w] = new_val;
                choice[i][w] = true;
            }
        }
    }

    // reconstruct chosen items
    let mut selected_items = Vec::new();
    let mut unselected_items = Vec::new();
    let mut w = w_max;

    for i in (0..n).rev() {
        let weight = items[i].0 as usize;
        if w >= weight && choice[i][w] {
            selected_items.push(items[i].clone());
            w -= weight;
        } else {
            unselected_items.push(items[i].clone());
        }
    }

    selected_items.reverse();
    unselected_items.reverse();

    (selected_items, unselected_items)
}

pub fn skip_if_no_market_activity(live_orders: &OrderList<OrderWithUser>) -> (bool, String) {
    let sell_count = live_orders.sell_orders.len();
    let buy_count = live_orders.buy_orders.len();

    if sell_count == 0 || buy_count == 0 {
        let operation = if sell_count == 0 { "selling" } else { "buying" };
        return (true, operation.to_string());
    }
    (false, "".to_string())
}

pub async fn collect_interesting_items(
    component: impl Into<String>,
    settings: &Settings,
) -> Result<Vec<ItemEntry>, Error> {
    let component = component.into();
    let conn = DATABASE.get().unwrap();
    // Variables.
    let stock_item_settings = &settings.live_scraper.stock_item;
    let mut interesting_items: HashMap<String, ItemEntry> = HashMap::new();

    // -- Debugging Mode --
    if !settings.debugging.live_scraper.entries.is_empty() {
        debug(
            format!("{}Debug", component),
            "Debugging enabled for live scraper will use predefined entries",
            &LoggerOptions::default(),
        );
        return Ok(settings.debugging.live_scraper.entries.clone());
    }

    // --- Buy Mode ---
    if settings.live_scraper.has_trade_mode(TradeMode::Buy) {
        let buy_list = get_interesting_items(
            &settings.live_scraper.stock_item,
            settings.live_scraper.low_rank_mods_rankless_mode,
        );
        for item in buy_list {
            let item_entry = ItemEntry::from(&item)
                .set_buy_quantity(settings.live_scraper.stock_item.buy_quantity);
            if !stock_item_settings.is_item_blacklisted(&item.wfm_id, &TradeMode::Buy) {
                interesting_items.insert(item_entry.uuid().clone(), item_entry);
            }
        }
    }

    // --- Sell Mode ---
    if settings.live_scraper.has_trade_mode(TradeMode::Sell) {
        let stock_items = StockItemQuery::get_all(conn, StockItemPaginationQueryDto::new(1, -1))
            .await
            .map_err(|e| e.with_location(get_location!()))?;
        for item in stock_items.results {
            if !stock_item_settings.is_item_blacklisted(&item.wfm_id, &TradeMode::Sell) {
                interesting_items
                    .entry(item.uuid())
                    .and_modify(|entry| {
                        entry.priority = 1;
                        entry.sell_quantity = item.owned;
                        entry.stock_id = Some(item.id);
                        entry.operation.push("Sell".to_string());
                    })
                    .or_insert_with(|| ItemEntry::from(&item).set_sell_quantity(item.owned));
            }
        }
    }

    // --- WishList Mode ---
    if settings.live_scraper.has_trade_mode(TradeMode::WishList) {
        let wish_items = WishListQuery::get_all(conn, WishListPaginationQueryDto::new(1, -1))
            .await
            .map_err(|e| e.with_location(get_location!()))?;
        for item in wish_items.results {
            if !stock_item_settings.is_item_blacklisted(&item.wfm_id, &TradeMode::WishList) {
                interesting_items
                    .entry(item.uuid())
                    .and_modify(|entry| {
                        entry.priority = 2;
                        entry.buy_quantity = item.quantity;
                        entry.wish_list_id = Some(item.id);
                        entry.operation.push("WishList".to_string());
                    })
                    .or_insert_with(|| ItemEntry::from(&item));
            }
        }
    }
    Ok(interesting_items.into_values().collect())
}

fn is_same_sub_type_without_rank(
    left: &Option<EntitySubType>,
    right: &Option<EntitySubType>,
) -> bool {
    let left = left.clone().unwrap_or_default();
    let right = right.clone().unwrap_or_default();
    left.variant == right.variant
        && left.charges == right.charges
        && left.amber_stars == right.amber_stars
        && left.cyan_stars == right.cyan_stars
}

fn find_existing_order_by_mode(
    item_info: &CacheTradableItem,
    entry: &ItemEntry,
    wfm_client: &wf_market::Client<wf_market::Authenticated>,
    order_type: OrderType,
    settings: &Settings,
) -> Option<wf_market::types::Order> {
    if !should_ignore_rank_for_low_rank_mod(settings, item_info) {
        return wfm_client.order().cache_orders().find_order(
            &item_info.wfm_id,
            &SubTypeExt::from_entity(entry.sub_type.clone()),
            order_type,
        );
    }

    let orders = wfm_client.order().cache_orders();
    let candidates = match order_type {
        OrderType::Buy => &orders.buy_orders,
        OrderType::Sell => &orders.sell_orders,
    };

    candidates
        .iter()
        .find(|order| {
            order.item_id == item_info.wfm_id
                && is_same_sub_type_without_rank(
                    &entry.sub_type,
                    &order.subtype.to_entity(),
                )
        })
        .cloned()
}

pub fn get_order_info(
    item_info: &CacheTradableItem,
    entry: &ItemEntry,
    wfm_client: &wf_market::Client<wf_market::Authenticated>,
    order_type: OrderType,
    settings: &Settings,
) -> OrderDetails {
    let quantity = if order_type == OrderType::Buy {
        entry.buy_quantity
    } else {
        entry.sell_quantity
    };
    find_existing_order_by_mode(item_info, entry, wfm_client, order_type, settings)
        .map(|order| {
            order
                .get_details()
                .set_operation(&["Update"])
                .set_order_id(&order.id)
                .set_update_string(&order.update_string())
        })
        .unwrap_or_default()
        .set_item_id(&item_info.wfm_id)
        .set_quantity(quantity as u32)
        .set_sub_type(entry.sub_type.clone())
        .set_info(item_info)
}

async fn handler_wfm_error(
    wfm_client: &wf_market::Client<wf_market::Authenticated>,
    component: &str,
    action: &str,
    message: &str,
    options: &LoggerOptions,
    e: wf_market::errors::ApiError,
) -> utils::Error {
    let log_level = match e {
        wf_market::errors::ApiError::AuctionLimitExceeded(_) => LogLevel::Warning,
        wf_market::errors::ApiError::OrderLimitExceededSamePrice(_)
        | wf_market::errors::ApiError::NotFound(_)
        | wf_market::errors::ApiError::OrderLimitExceeded(_) => {
            wfm_client.order().my_orders().await.ok();
            wfm_client
                .order()
                .cache_orders_mut()
                .apply_trade_info()
                .ok();
            trace(
                format!("{}:{}", component, action),
                "Refreshed cached orders due to order limit exceeded",
                options,
            );
            LogLevel::Warning
        }
        _ => LogLevel::Error,
    };
    let mut err = Error::from_wfm(
        format!("{}:{}", component, action),
        message.to_string(),
        e,
        get_location!(),
    );
    err = err.set_log_level(log_level);
    err
}

pub async fn progress_order(
    component: &str,
    wfm_client: &wf_market::Client<wf_market::Authenticated>,
    order_info: &OrderDetails,
    order_type: OrderType,
    post_price: u32,
    per_trade: Option<u32>,
    log_options: &LoggerOptions,
) -> Result<(), Error> {
    let can_create_order = wfm_client.order().can_create_order();
    let file_name = "progress_order.log";
    if order_info.has_operation("Create") && !order_info.has_operation("Delete") && can_create_order
    {
        match wfm_client
            .order()
            .create(
                CreateOrderParams::new_with_subtype(
                    &order_info.item_id,
                    order_type,
                    post_price,
                    order_info.quantity,
                    true,
                    per_trade,
                    SubTypeExt::from_entity(order_info.sub_type.clone()),
                )
                .with_properties(json!(order_info)),
            )
            .await
        {
            Ok(order) => {
                info(
                    format!("{}CreateSuccess", component),
                    &format!(
                        "Created order for item {}: {}",
                        order_info.item_name, order.id
                    ),
                    &log_options,
                );
                send_event!(UIEvent::RefreshWfmOrders, json!({"source": component}));
            }
            Err(e) => {
                let err = handler_wfm_error(
                    wfm_client,
                    component,
                    "Create",
                    &format!("Failed to create order for item {}", order_info.item_name),
                    log_options,
                    e,
                )
                .await
                .with_location(get_location!())
                .log_with_options(file_name, &log_options);
                return Err(err);
            }
        }
    } else if order_info.has_operation("Update") && !order_info.has_operation("Delete") {
        match wfm_client
            .order()
            .update(
                &order_info.order_id,
                UpdateOrderParams::new()
                    .with_platinum(post_price)
                    .with_quantity(order_info.quantity)
                    .with_per_trade(per_trade)
                    .with_properties(json!(order_info)),
            )
            .await
        {
            Ok(order) => {
                info(
                    format!("{}UpdateSuccess", component),
                    &format!(
                        "Updated order for item {}: {}",
                        order_info.item_name, order_info.order_id
                    ),
                    &log_options,
                );
                if order.update_string() != order_info.update_string {
                    send_event!(UIEvent::RefreshWfmOrders, json!({"source": component}));
                }
            }
            Err(e) => {
                let err = handler_wfm_error(
                    wfm_client,
                    component,
                    "Update",
                    &format!("Failed to update order for item {}", order_info.item_name),
                    log_options,
                    e,
                )
                .await
                .with_location(get_location!())
                .log_with_options(file_name, &log_options);
                return Err(err);
            }
        }
    } else if order_info.has_operation("Update") && order_info.has_operation("Delete") {
        match wfm_client.order().delete(&order_info.order_id).await {
            Ok(_) => {
                info(
                    format!("{}DeleteSuccess", component),
                    &format!(
                        "Deleted order for item {}: {}",
                        order_info.item_name, order_info.order_id
                    ),
                    &log_options,
                );
                send_event!(UIEvent::RefreshWfmOrders, json!({"source": component}));
            }
            Err(e) => {
                let err = handler_wfm_error(
                    wfm_client,
                    component,
                    "Delete",
                    &format!("Failed to delete order for item {}", order_info.item_name),
                    log_options,
                    e,
                )
                .await
                .with_location(get_location!())
                .log_with_options(file_name, &log_options);
                return Err(err);
            }
        }
    } else if !can_create_order {
        warning(
            format!("{}Skip", component),
            &format!(
                "Item {} has reached the order limit. Skipping.",
                order_info.item_name
            ),
            &log_options,
        );
    } else {
        warning(
            format!("{}Skip", component),
            &format!(
                "Item {} is not optimal for buying. Skipping.",
                order_info.item_name
            ),
            &log_options,
        );
    }
    Ok(())
}

pub async fn fetch_and_cache_orders(
    component: &str,
    wfm_client: &wf_market::Client<wf_market::Authenticated>,
    item_url: &str,
    cache_path: Option<&Path>,
) -> Result<OrderList<OrderWithUser>, Error> {
    let orders = wfm_client
        .order()
        .get_orders_by_item(item_url)
        .await
        .map_err(|e| {
            let log_level = match e {
                wf_market::errors::ApiError::RequestError(_) => LogLevel::Error,
                _ => LogLevel::Critical,
            };
            Error::from_wfm(
                format!("{}:FetchAndCacheOrders", component),
                &format!("Failed to get live orders for item {}", item_url),
                e,
                get_location!(),
            )
            .set_log_level(log_level)
        })?;

    if let Some(path) = cache_path {
        utils::write_json_file(path, &orders)?;
    }

    Ok(orders)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_price(rank: i64, volume: f64, avg_price: f64) -> ItemPriceInfo {
        ItemPriceInfo {
            wfm_url: "test_mod".to_string(),
            wfm_id: "123".to_string(),
            uuid: format!("test_mod-R {}", rank),
            volume,
            max_price: avg_price + 2.0,
            min_price: avg_price - 2.0,
            avg_price,
            moving_avg: Some(avg_price + 1.0),
            median: avg_price,
            profit: 10.0,
            profit_margin: 25.0,
            trading_tax: 2000,
            week_price_shift: 3.0,
            sub_type: Some(EntitySubType::rank(rank)),
        }
    }

    #[test]
    fn aggregate_rankless_profile_uses_weighted_averages() {
        let prices = vec![sample_price(0, 10.0, 20.0), sample_price(3, 30.0, 40.0)];
        let aggregated = aggregate_rankless_item_price(&prices).expect("should aggregate");

        assert_eq!(aggregated.wfm_id, "123");
        assert_eq!(aggregated.sub_type.and_then(|sub| sub.rank), Some(0));
        assert!((aggregated.avg_price - 35.0).abs() < f64::EPSILON);
        assert!((aggregated.moving_avg.unwrap_or_default() - 36.0).abs() < f64::EPSILON);
    }

    #[test]
    fn low_rank_mod_flag_is_respected() {
        let item = CacheTradableItem {
            name: "Test".to_string(),
            unique_name: "Test".to_string(),
            wfm_id: "123".to_string(),
            wfm_url_name: "test_mod".to_string(),
            trade_tax: 2000,
            mr_requirement: 0,
            tags: vec!["mod".to_string()],
            wiki_url: "".to_string(),
            image_url: "".to_string(),
            max_rank: Some(5),
            bulk_tradable: false,
            sub_type: None,
        };

        assert!(should_ignore_rank_for_low_rank_mod_with_flag(true, &item));
        assert!(!should_ignore_rank_for_low_rank_mod_with_flag(false, &item));
    }
}
