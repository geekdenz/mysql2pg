<?php

namespace BookStack\Console\Commands;

use Illuminate\Console\Command;
use Illuminate\Support\Facades\DB;

/**
 * Drives Laravel's query builder through a write/read/aggregate cycle.
 *
 * The query builder rather than the Eloquent models on purpose: what is being
 * proved is the SQL Laravel's MySQL grammar generates, and BookStack reshuffles
 * which models map to which tables between releases. `entities` is BookStack's
 * own table, created by its own migrations.
 */
class Mysql2pgProof extends Command
{
    protected $signature = 'mysql2pg:proof';

    protected $description = 'Creates, queries and aggregates rows through Laravel\'s query builder';

    public function handle(): int
    {
        DB::table('entities')->where('name', 'like', 'mysql2pg%')->delete();

        $now = '2026-09-25 12:00:00';
        $bookId = DB::table('entities')->insertGetId([
            'type' => 'book', 'name' => 'mysql2pg book', 'slug' => 'mysql2pg-book',
            'priority' => 0, 'created_at' => $now, 'updated_at' => $now,
            'created_by' => 1, 'updated_by' => 1, 'owned_by' => 1,
        ]);

        foreach ([['mysql2pg page one', 10], ['mysql2pg page two', 20], ['mysql2pg page three', 30]] as [$name, $priority]) {
            DB::table('entities')->insert([
                'type' => 'page', 'name' => $name, 'slug' => str_replace(' ', '-', $name),
                'book_id' => $bookId, 'priority' => $priority,
                'created_at' => $now, 'updated_at' => $now,
                'created_by' => 1, 'updated_by' => 1, 'owned_by' => 1,
            ]);
        }

        $pages = fn () => DB::table('entities')->where('type', 'page')->where('book_id', $bookId);

        $total   = $pages()->count();
        $sum     = (int) $pages()->sum('priority');
        $max     = (int) $pages()->max('priority');
        $top     = $pages()->orderByDesc('priority')->first();
        $partial = DB::table('entities')->where('name', 'like', '%page two%')->first();

        // UPDATE then re-read, covering the write-through path.
        DB::table('entities')->where('id', $top->id)->update(['priority' => 99]);
        $reread = DB::table('entities')->find($top->id);

        // A JOIN with GROUP BY, where dialect differences usually bite.
        $grouped = DB::table('entities as p')
            ->join('entities as b', 'b.id', '=', 'p.book_id')
            ->where('b.id', $bookId)
            ->where('p.type', 'page')
            ->groupBy('b.name')
            ->selectRaw('b.name as book_name, count(*) as page_count')
            ->first();

        // A subquery with an aggregate, and ordering by it.
        $withCounts = DB::table('entities as b')
            ->where('b.id', $bookId)
            ->select('b.name')
            ->selectSub(
                DB::table('entities as c')->whereColumn('c.book_id', 'b.id')->selectRaw('count(*)'),
                'child_count'
            )
            ->first();

        $this->line(sprintf(
            'PROOF book=%s pages=%d sum_priority=%d max_priority=%d top=%s partial=%s updated=%d grouped=%s/%d subquery=%d',
            'mysql2pg-book',
            $total,
            $sum,
            $max,
            $top->name,
            $partial->name ?? 'none',
            $reread->priority,
            $grouped->book_name ?? 'none',
            $grouped->page_count ?? 0,
            $withCounts->child_count ?? 0
        ));

        return self::SUCCESS;
    }
}
