"use strict";

ObjC.import("Foundation");
ObjC.bindFunction("getchar", ["int", []]);

const reply = (value) => {
    const data = $(`${JSON.stringify(value)}\n`).dataUsingEncoding($.NSUTF8StringEncoding);
    $.NSFileHandle.fileHandleWithStandardOutput.writeData(data);
};

this.run = (argv) => {
    try {
        const ghostty = Application("com.mitchellh.ghostty");
        const matches = ghostty.terminals.whose({tty: argv[0]})();
        if (matches.length !== 1) {
            throw new Error(`Ghostty terminal not found for ${argv[0]}`);
        }
        const terminal = ghostty.terminals.byId(matches[0].id());
        reply(true);
        let line = "";
        for (let byte = $.getchar(); byte !== -1; byte = $.getchar()) {
            if (byte !== 10) {
                line += String.fromCharCode(byte);
                continue;
            }
            const title = JSON.parse(decodeURIComponent(escape(line)));
            line = "";
            if (!ghostty.performAction(`set_surface_title:${title}`, {on: terminal})) {
                throw new Error("Ghostty rejected the title update");
            }
            reply(true);
        }
    } catch (error) {
        reply(String(error));
    }
};
