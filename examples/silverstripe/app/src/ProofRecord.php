<?php

namespace App\Model;

use SilverStripe\ORM\DataObject;

/**
 * A deliberately boring DataObject.
 *
 * Its only job is to make SilverStripe's ORM exercise a spread of column types
 * and query shapes against the middleware: string, integer, decimal, boolean,
 * text and datetime, plus the indexes SilverStripe adds of its own accord.
 */
class ProofRecord extends DataObject
{
    private static $table_name = 'ProofRecord';

    private static $db = [
        'Title' => 'Varchar(255)',
        'Quantity' => 'Int',
        'Price' => 'Decimal(10,2)',
        'Active' => 'Boolean',
        'Notes' => 'Text',
        'PublishedAt' => 'Datetime',
    ];

    private static $indexes = [
        'Title' => true,
    ];

    private static $summary_fields = [
        'Title', 'Quantity', 'Price', 'Active',
    ];
}
