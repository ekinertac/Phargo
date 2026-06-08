<?php
    class LocalTerms { const GOV_LEADER = "David William Donald Cameron"; }
    require '-Officials.inc';
    printf( "The Prime Minister of the UK is %s\n", Officials::getLeader() );
    