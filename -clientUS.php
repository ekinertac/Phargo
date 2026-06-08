<?php
    class LocalTerms { const GOV_LEADER = "Barack Hussein Obama II"; }
    require '-Officials.inc';
    printf( "The President of the USA is %s\n", Officials::getLeader() );
    