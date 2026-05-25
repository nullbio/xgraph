<?php

namespace App\Models;

enum Status: string
{
    case Active = 'active';
    case Inactive = 'inactive';

    public function label(): string
    {
        return match ($this) {
            Status::Active => 'Active',
            Status::Inactive => 'Inactive',
        };
    }
}
