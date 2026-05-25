<?php

namespace App\Util;

function dispatch(string $event, $handler): string
{
    $label = sprintf('event=%s', strtolower($event));
    $handler->handle($label);
    Container::resolve('logger')->log($label);
    $handler?->name();
    return $label;
}
