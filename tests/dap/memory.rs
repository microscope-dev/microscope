//! Memory, disassembly, and instruction breakpoints.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, Stopped, fixture, line_of, source};

/// Launches `basic` stopped in `breakpoint_target`.
fn stopped_in_target(dap: &mut Dap) -> Stopped {
    let started = dap.launch(
        Profile::VsCode,
        &fixture("basic"),
        json!({}),
        &Configuration {
            functions: vec!["breakpoint_target".to_owned()],
            ..Configuration::default()
        },
    );
    dap.stopped(started.mark)
}

fn address(value: &Value) -> u64 {
    let text = value.as_str().expect("an address string");
    u64::from_str_radix(text.trim_start_matches("0x"), 16).expect("hexadecimal address")
}

fn disassemble(dap: &mut Dap, reference: &Value, offset: i64, count: i64) -> Vec<Value> {
    dap.request(
        "disassemble",
        json!({"memoryReference": reference, "instructionOffset": offset, "instructionCount": count, "resolveSymbols": true}),
    )["instructions"]
        .as_array()
        .expect("instructions")
        .clone()
}

#[test]
fn memory_reads_return_readable_bytes_and_say_where_reading_stopped() {
    let mut dap = Dap::start("read memory");
    let stop = stopped_in_target(&mut dap);
    let frame =
        dap.request("stackTrace", json!({"threadId": stop.thread}))["stackFrames"][0].clone();
    let scopes = dap.request("scopes", json!({"frameId": frame["id"]}));
    let statics = scopes["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .find(|scope| scope["name"] == "Statics")
        .expect("a statics scope")
        .clone();
    assert_eq!(statics["expensive"], true);
    let globals = dap.request(
        "variables",
        json!({"variablesReference": statics["variablesReference"]}),
    );
    let value = globals["variables"]
        .as_array()
        .expect("statics")
        .iter()
        .find(|variable| variable["name"] == "microscope_value")
        .expect("the file's global")
        .clone();
    assert_eq!(value["value"], "1234605616436508552");
    let read = dap.request(
        "readMemory",
        json!({"memoryReference": value["memoryReference"], "count": 8}),
    );

    // Little-endian 0x1122334455667788.
    assert_eq!(read["data"], "iHdmVUQzIhE=");
    assert_eq!(read["address"], value["memoryReference"]);

    // Offsets and decimal references reach the same bytes.
    let decimal = address(&value["memoryReference"]).to_string();
    let offset = dap.request(
        "readMemory",
        json!({"memoryReference": decimal, "offset": 4, "count": 4}),
    );
    assert_eq!(offset["data"], "RDMiEQ==");

    // Find the end of a mapping that no other mapping follows.
    let pid = dap.process_id().expect("process");
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).expect("maps");
    let ranges = maps
        .lines()
        .map(|line| {
            let (start, end) = line
                .split_whitespace()
                .next()
                .expect("range")
                .split_once('-')
                .expect("dash");
            (
                u64::from_str_radix(start, 16).expect("start"),
                u64::from_str_radix(end, 16).expect("end"),
            )
        })
        .collect::<Vec<_>>();
    let end = ranges
        .iter()
        .map(|(_, end)| *end)
        .find(|end| !ranges.iter().any(|(start, _)| start == end))
        .expect("an isolated mapping end");
    let edge = dap.request(
        "readMemory",
        json!({"memoryReference": format!("{:#x}", end - 8), "count": 64}),
    );
    assert_eq!(
        edge["data"].as_str().expect("data").len(),
        12,
        "8 readable bytes: {edge}"
    );
    assert_eq!(edge["unreadableBytes"], 56);

    let zero = dap.request(
        "readMemory",
        json!({"memoryReference": format!("{:#x}", end - 8), "count": 0}),
    );
    assert_eq!(zero["data"], "");
    assert!(
        dap.request_error(
            "readMemory",
            json!({"memoryReference": "nowhere", "count": 4})
        )
        .contains("invalid memory reference 'nowhere'")
    );
    dap.finish();
}

#[test]
fn disassembly_returns_exactly_the_rows_asked_for_around_any_address() {
    let mut dap = Dap::start("disassemble");
    let stop = stopped_in_target(&mut dap);
    let trace = dap.request("stackTrace", json!({"threadId": stop.thread}));
    let pointer = trace["stackFrames"][0]["instructionPointerReference"].clone();
    let caller = trace["stackFrames"][1]["instructionPointerReference"].clone();

    // VS Code opens the view with 200 rows before the address and 200 from it.
    let rows = disassemble(&mut dap, &pointer, -200, 400);
    assert_eq!(rows.len(), 400);
    assert_eq!(rows[200]["address"], pointer);
    let addresses = rows
        .iter()
        .map(|row| address(&row["address"]))
        .collect::<Vec<_>>();
    assert!(
        addresses.windows(2).all(|pair| pair[0] < pair[1]),
        "rows ascend"
    );
    assert_eq!(rows[200]["symbol"], "breakpoint_target");
    assert!(rows[200]["line"].as_u64().is_some());

    // A source is named on the first row of each run of one file.
    let first_source = rows
        .iter()
        .find(|row| row.get("line").is_some())
        .expect("a row with source");
    assert!(
        first_source["location"]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("basic.c"))
    );

    // Padding rows are marked and never decoded from unproven starts.
    for row in &rows {
        if row["presentationHint"] == "invalid" {
            assert!(
                row["instruction"] == "??" || row["instruction"] == "(bad)",
                "{row}"
            );
        } else {
            assert!(
                row["instructionBytes"]
                    .as_str()
                    .is_some_and(|bytes| !bytes.is_empty())
            );
        }
    }

    // A direct call names its target.
    let call = disassemble(&mut dap, &caller, -1, 1);
    assert_eq!(call.len(), 1);
    let text = call[0]["instruction"].as_str().expect("text");
    assert!(
        text.starts_with("call") && text.contains("<breakpoint_target>"),
        "{text}"
    );

    // Scrolling up from the first row continues exactly before it.
    let first = rows[0]["address"].clone();
    let above = disassemble(&mut dap, &first, -50, 50);
    assert_eq!(above.len(), 50);
    assert!(address(&above[49]["address"]) < address(&first));

    // Scrolling down continues after the last row.
    let last = rows[399]["address"].clone();
    let below = disassemble(&mut dap, &last, 1, 50);
    assert_eq!(below.len(), 50);
    assert!(address(&below[0]["address"]) > address(&last));

    // An address no module maps is all padding.
    let nowhere = disassemble(&mut dap, &json!("0x10"), -2, 4);
    assert_eq!(nowhere.len(), 4);
    assert!(
        nowhere
            .iter()
            .all(|row| row["presentationHint"] == "invalid")
    );
    assert_eq!(
        dap.request(
            "disassemble",
            json!({"memoryReference": pointer, "instructionCount": 0})
        )["instructions"],
        json!([])
    );
    dap.finish();
}

#[test]
fn instruction_breakpoints_stop_at_their_address() {
    let mut dap = Dap::start("instruction breakpoints");
    let stop = stopped_in_target(&mut dap);
    let trace = dap.request("stackTrace", json!({"threadId": stop.thread}));
    let pointer = trace["stackFrames"][0]["instructionPointerReference"].clone();
    let rows = disassemble(&mut dap, &pointer, 0, 3);
    let target = rows[2]["address"].clone();

    // The reference and offset name the same instruction two ways.
    let offset = i64::try_from(address(&target) - address(&pointer)).expect("small offset");
    let set = dap.request(
        "setInstructionBreakpoints",
        json!({"breakpoints": [{"instructionReference": pointer, "offset": offset}, {"instructionReference": "bogus"}]}),
    );
    let [valid, invalid] = &crate::dap::breakpoints(&set)[..] else {
        panic!("two breakpoints");
    };

    assert_eq!(
        (&valid["verified"], &valid["instructionReference"]),
        (&json!(true), &target)
    );
    assert_eq!(invalid["verified"], false);
    assert_eq!(
        invalid["message"],
        "invalid instruction reference 'bogus' with offset 0"
    );
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    let hit = dap.stopped(resumed.mark);
    assert_eq!(hit.reason, "instruction breakpoint");
    assert_eq!(hit.body["hitBreakpointIds"], json!([valid["id"]]));
    let trace = dap.request("stackTrace", json!({"threadId": hit.thread, "levels": 1}));
    assert_eq!(
        trace["stackFrames"][0]["instructionPointerReference"],
        target
    );
    dap.finish();
}

#[test]
fn disassembly_crosses_into_a_functions_split_off_cold_part() {
    let program = fixture("crash-gcc-o2-nopie");
    let data = std::fs::read(&program).expect("read the program");
    let file = object::File::parse(&*data).expect("parse the program");
    let cold = object::Object::symbols(&file)
        .find(|symbol| object::ObjectSymbol::name(symbol) == Ok("main.cold"))
        .map(|symbol| object::ObjectSymbol::address(&symbol))
        .expect("gcc split main's cold path");

    let mut dap = Dap::start("cold disassembly");
    let started = dap.begin(
        Profile::VsCode,
        (
            "attach",
            json!({"coreFile": fixture("crash-gcc-o2-nopie-segv.core"), "program": program}),
        ),
        &Configuration::default(),
    );
    dap.stopped(started.mark);
    let rows = disassemble(&mut dap, &json!(format!("{cold:#x}")), -8, 40);
    assert_eq!(rows.len(), 40);
    let addresses = rows
        .iter()
        .map(|row| address(&row["address"]))
        .collect::<Vec<_>>();
    assert!(
        addresses.windows(2).all(|pair| pair[0] < pair[1]),
        "rows ascend"
    );
    assert_eq!(addresses[8], cold);

    // The cold part is named by its own symbol, and its line is the branch
    // of main that gcc moved there.
    assert_eq!(rows[8]["symbol"], "main.cold");
    assert!(rows[..8].iter().all(|row| row["symbol"] != "main.cold"));
    assert_eq!(
        rows[8]["line"],
        line_of(&source("c/crash/main.c"), "crash_abort(&record);")
    );
    dap.finish();
}
