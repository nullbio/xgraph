import defaultExport from 'mod-default';
import { a, b } from 'mod-named';
import * as ns from 'mod-namespace';
import 'mod-side-effect';

const required = require('mod-required');

export const named = 1;
export default function topLevel() {
    return required(named);
}

class Container {
    process(value) {
        return ns.transform(value);
    }
}

const make = (input) => new Container().process(input);
