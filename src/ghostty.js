"use strict";

ObjC.import("AppKit");
ObjC.bindFunction("getchar", ["int", []]);

const code = text => Array.from(text).reduce((value, character) => value * 256 + character.charCodeAt(0), 0);
const descriptor = $.NSAppleEventDescriptor;
const reply = (value) => {
    const data = $(`${JSON.stringify(value)}\n`).dataUsingEncoding($.NSUTF8StringEncoding);
    $.NSFileHandle.fileHandleWithStandardOutput.writeData(data);
};

const sendEvent = (pid, eventClass, eventID, parameters) => {
    const target = descriptor.descriptorWithProcessIdentifier(pid);
    const event = descriptor.appleEventWithEventClassEventIDTargetDescriptorReturnIDTransactionID(
        code(eventClass), code(eventID), target, -1, 0
    );
    for (const [keyword, value] of parameters) {event.setParamDescriptorForKeyword(value, code(keyword));}
    const waitReplyNeverInteractDontReconnect = 0x03 | 0x10 | 0x80;
    const result = event.sendEventWithOptionsTimeoutError(waitReplyNeverInteractDontReconnect, 1, Ref());
    if (!result || result.isNil()) {throw new Error("Ghostty did not answer the Apple event");}
    const failure = result.paramDescriptorForKeyword(code("errn"));
    if (failure && !failure.isNil() && failure.int32Value !== 0) {
        throw new Error(`Ghostty Apple event error ${failure.int32Value}`);
    }
    return result.paramDescriptorForKeyword(code("----"));
};

const specifier = (want, form, selection, container) => {
    const record = descriptor.recordDescriptor;
    record.setDescriptorForKeyword(descriptor.descriptorWithTypeCode(code(want)), code("want"));
    record.setDescriptorForKeyword(descriptor.descriptorWithEnumCode(code(form)), code("form"));
    record.setDescriptorForKeyword(selection, code("seld"));
    record.setDescriptorForKeyword(container, code("from"));
    return record.coerceToDescriptorType(code("obj "));
};

const terminals = pid => sendEvent(pid, "core", "getd", [["----", specifier(
    "Gtrm", "indx", descriptor.descriptorWithDescriptorTypeData(code("abso"), descriptor.descriptorWithEnumCode(code("all ")).data), descriptor.nullDescriptor
)]]);

const connectGhostty = tty => {
    const running = $.NSRunningApplication.runningApplicationsWithBundleIdentifier("com.mitchellh.ghostty");
    for (let index = 0; index < running.count; index++) {
        const pid = running.objectAtIndex(index).processIdentifier;
        const surfaces = terminals(pid);
        for (let surfaceIndex = 1; surfaceIndex <= surfaces.numberOfItems; surfaceIndex++) {
            const surface = surfaces.descriptorAtIndex(surfaceIndex);
            const terminalTty = sendEvent(pid, "core", "getd", [["----", specifier(
                "prop", "prop", descriptor.descriptorWithTypeCode(code("Gtty")), surface
            )]]);
            if (ObjC.unwrap(terminalTty.stringValue) === tty) {
                return {setTitle: title => sendEvent(pid, "Ghst", "PfAc", [
                    ["----", descriptor.descriptorWithString(`set_surface_title:${title}`)],
                    ["GonT", surface]
                ]).booleanValue};
            }
        }
    }
    throw new Error(`Ghostty terminal not found for ${tty}`);
};

this.run = (argv) => {
    try {
        const terminal = connectGhostty(argv[0]);
        reply(true);
        let line = "";
        for (let byte = $.getchar(); byte !== -1; byte = $.getchar()) {
            if (byte !== 10) {
                line += String.fromCharCode(byte);
                continue;
            }
            const title = JSON.parse(decodeURIComponent(escape(line)));
            line = "";
            if (!terminal.setTitle(title)) {
                throw new Error("Ghostty rejected the title update");
            }
            reply(true);
        }
    } catch (error) {
        reply(String(error));
    }
};
