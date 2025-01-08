#![allow(non_snake_case)]
use proc_macro::TokenStream;
use quote::{format_ident, quote};
use serde::{Deserialize, Serialize};
use syn::parse::Parser;
use syn::Stmt;
use syn::{parse_macro_input, ItemStruct};
use syn::{ImplItem, ItemImpl};

#[derive(Debug, Default, Serialize, Deserialize)]
struct ComponentConfig {
    port: Option<ComponentPortConfig>,
    // serde default on bool is false
    #[serde(default)]
    is_primary: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ComponentPortConfig {
    input: Option<Vec<(String, String)>>,
    output: Option<Vec<(String, String)>>,
    #[serde(default)]
    clock: bool,
}

/// Preprocessor for a component
///
/// The proc macro adds the following to the struct field:
/// 1. `component_id: ComponentId`
/// 2. `sim_manager: Arc<Mutex<SimManager`
/// 3. `ack_sender: Sender<EventId>`
/// 4. `${port_name}_{receiver/sender}: Input/Output`
/// 5. `${port_name}: ${port_type}`
/// 6. `${port_name}_old: ${port_type}` to prevent circular dependency
/// 7. `clock_sender: Output` and `clock_receiver: Input` if the component has a clock
///
/// For each port, the proc macro will also generate an implementation of `poll_recv`
/// - The data will be extracted from the received event from `${port_name}_{receiver}` and put into `${port_name}`
/// - `on_comb` will be invoked
/// - If the port is clock, `on_clock` will also be invoked prior to `on_comb`
///
/// User should write the impl for the following functions:
/// - `init_impl(&mut self)`
/// - `reset_impl(&mut self)`
/// - `poll_impl(&mut self)`
/// - `on_comb(&mut self)`
/// - `on_clock(&mut self)`
#[proc_macro_attribute]
pub fn ComponentAttribute(config: TokenStream, input: TokenStream) -> TokenStream {
    let mut item_struct = parse_macro_input!(input as ItemStruct);
    let struct_name = item_struct.ident.clone();

    let component_impl_ts = quote! {
        impl Component for #struct_name {
            fn init(&mut self) { self.init_impl(); }

            fn reset(&mut self) { self.reset_impl(); }

            fn poll_recv(&mut self) { self.poll_impl(); }

            fn get_component_id(&self) -> ComponentId { self.component_id }
        }
    }
    .into();

    let mut component_impl_item = parse_macro_input!(component_impl_ts as ItemImpl);

    let component_config: ComponentConfig = serde_json::from_str(&config.to_string()).unwrap();

    // Every component should have these values
    let mut extended_field = vec![
        syn::Field::parse_named
            .parse2(quote! { component_id: rsim_core::types::ComponentId })
            .unwrap(),
        syn::Field::parse_named
            .parse2(quote! { sim_manager: Arc<SimManager> })
            .unwrap(),
        syn::Field::parse_named
            .parse2(quote! { ack_sender: crossbeam_channel::Sender<EventId> })
            .unwrap(),
    ];

    if component_config.is_primary {
        let _ = component_impl_item
            .items
            .iter_mut()
            .map(|item| {
                if let ImplItem::Fn(func) = item {
                    if func.sig.ident == format_ident!("init") {
                        func.block.stmts.push(syn::parse_quote! {self.sim_manager.register_do_not_end(self.get_component_id());})
                    }
                }
            })
            .collect::<Vec<_>>();
    }

    if let Some(port) = component_config.port {
        // If the component has clock, we need to
        // 1. register the clock with the sim manager
        // 2. call on_clock when clock ticks
        if port.clock {
            extended_field.extend(vec![
                syn::Field::parse_named
                    .parse2(quote! { clock_sender: Output })
                    .unwrap(),
                syn::Field::parse_named
                    .parse2(quote! { clock_receiver: Input })
                    .unwrap(),
            ]);
            let _ = component_impl_item
                .items
                .iter_mut()
                .map(|item| {
                    if let ImplItem::Fn(func) = item {
                        if func.sig.ident == format_ident!("init") {
                            func.block.stmts.push(syn::parse_quote! {self.sim_manager
                            .register_clock_tick(self.clock_sender.clone());})
                        } else if func.sig.ident == format_ident!("poll_recv") {
                            push_clock_recv_stmt(&mut func.block.stmts)
                        }
                    }
                })
                .collect::<Vec<_>>();
        }
        // For each input port, it will have
        // 1. a mpsc receiver
        // 2. a variable holding the value
        // 3. a corresponding try_recv in poll_recv, acks and calls on_comb if successful
        port.input.map(|input| {
            input
                .iter()
                .map(|(port_name, port_type)| {
                    let rx = format_ident!("{}", port_name);
                    let rx_type: proc_macro2::TokenStream = port_type.parse().unwrap();
                    extended_field.extend(vec![syn::Field::parse_named
                        .parse2(quote! { pub #rx: rsim_core::rx::Rx<#rx_type> })
                        .unwrap()]);
                    let _ = component_impl_item
                        .items
                        .iter_mut()
                        .map(|item| {
                            if let ImplItem::Fn(func) = item {
                                if func.sig.ident == format_ident!("poll_recv") {
                                    push_comb_recv_stmt(&mut func.block.stmts, port_name)
                                } else if func.sig.ident == format_ident!("reset") {
                                    push_reset_stmt(&mut func.block.stmts, port_name)
                                }
                            }
                        })
                        .collect::<Vec<_>>();
                })
                .collect::<Vec<_>>()
        });
        // We assume outputs are not registered
        port.output.map(|output| {
            output
                .iter()
                .map(|(port_name, port_type)| {
                    let tx = format_ident!("{}", port_name);
                    let tx_type: proc_macro2::TokenStream = port_type.parse().unwrap();
                    extended_field.extend(vec![syn::Field::parse_named
                        .parse2(quote! { pub #tx: rsim_core::tx::Tx<#tx_type> })
                        .unwrap()])
                })
                .collect::<Vec<_>>()
        });
    };

    if let syn::Fields::Named(ref mut fields) = item_struct.fields {
        fields.named.extend(extended_field);
    }

    // println!("{:?}", item_struct);
    // println!("{:?}", component_impl_item);

    (quote! {
        #item_struct

        #component_impl_item
    })
    .into()
}

fn push_clock_recv_stmt(stmt: &mut Vec<Stmt>) {
    let receiver = format_ident!("clock_receiver");

    stmt.push(syn::parse_quote! {
        if let Ok(event) = self.#receiver.try_recv() {
            self.on_clock();
            self.on_comb();
            self.ack_sender.send(event.get_event_id()).unwrap();
        }
    })
}

fn push_comb_recv_stmt(stmt: &mut Vec<Stmt>, port_name: &str) {
    let rx = format_ident!("{}", port_name);

    stmt.push(syn::parse_quote! {
        let recv_result = self.#rx.try_recv();
    });
    stmt.push(syn::parse_quote! {
        if recv_result == rsim_core::rx::RxType::NewValue {
            self.on_comb();
        }
    });
    stmt.push(syn::parse_quote! {
        if recv_result != rsim_core::rx::RxType::NoValue{
            self.#rx.ack();
        }
    });
}

fn push_reset_stmt(stmt: &mut Vec<Stmt>, port_name: &str) {
    let rx = format_ident!("{}", port_name);

    stmt.push(syn::parse_quote! {
        self.#rx.reset();
    });
}
