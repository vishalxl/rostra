use rostra_util_error::BoxedErrorResult;

use crate::tests::temp_db_rng;
use crate::{Database, def_table};

def_table!(test_table: u64 => String);

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn paginate() -> BoxedErrorResult<()> {
    let (_dir, db) = temp_db_rng().await?;

    db.write_with(|tx| {
        let mut table = tx.open_table(&test_table::TABLE)?;

        table.insert(&0, "Zero")?;
        table.insert(&3, "Three")?;
        table.insert(&7, "Seven")?;

        assert_eq!(
            Database::paginate_table(&table, None, 0, |k, v| Ok(Some(format!("{k}-{v}"))))?,
            (vec![], Some(0))
        );

        assert_eq!(
            Database::paginate_table(&table, None, 1, |k, v| Ok(Some(format!("{k}-{v}"))))?,
            (vec!["0-Zero".into()], Some(3))
        );

        assert_eq!(
            Database::paginate_table(&table, Some(0), 0, |k, v| Ok(Some(format!("{k}-{v}"))))?,
            (vec![], Some(0))
        );

        assert_eq!(
            Database::paginate_table(&table, Some(0), 1, |k, v| Ok(Some(format!("{k}-{v}"))))?,
            (vec!["0-Zero".into()], Some(3))
        );

        assert_eq!(
            Database::paginate_table(&table, Some(3), 2, |k, v| Ok(Some(format!("{k}-{v}"))))?,
            (vec!["3-Three".into(), "7-Seven".into(),], None)
        );

        assert_eq!(
            Database::paginate_table(&table, Some(3), 3, |k, v| Ok(Some(format!("{k}-{v}"))))?,
            (vec!["3-Three".into(), "7-Seven".into(),], None)
        );

        assert_eq!(
            Database::paginate_table(&table, Some(40), 3, |k, v| Ok(Some(format!("{k}-{v}"))))?,
            (vec![], None)
        );

        assert_eq!(
            Database::paginate_table(&table, None, 2, |k, v| Ok((k % 2 == 0).then_some(v)))?,
            (vec!["Zero".into()], None)
        );

        Ok(())
    })
    .await?;

    Ok(())
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn paginate_rev() -> BoxedErrorResult<()> {
    let (_dir, db) = temp_db_rng().await?;

    db.write_with(|tx| {
        let mut table = tx.open_table(&test_table::TABLE)?;

        table.insert(&0, "Zero")?;
        table.insert(&3, "Three")?;
        table.insert(&7, "Seven")?;

        assert_eq!(
            Database::paginate_table_rev(&table, None, 0, |k, v| Ok(Some(format!("{k}-{v}"))))?,
            (vec![], Some(7))
        );

        assert_eq!(
            Database::paginate_table_rev(&table, None, 1, |k, v| Ok(Some(format!("{k}-{v}"))))?,
            (vec!["7-Seven".into()], Some(3))
        );

        assert_eq!(
            Database::paginate_table_rev(&table, Some(7), 0, |k, v| Ok(Some(format!("{k}-{v}"))))?,
            (vec![], Some(7))
        );

        assert_eq!(
            Database::paginate_table_rev(&table, Some(7), 1, |k, v| Ok(Some(format!("{k}-{v}"))))?,
            (vec!["7-Seven".into()], Some(3))
        );

        assert_eq!(
            Database::paginate_table_rev(&table, Some(3), 2, |k, v| Ok(Some(format!("{k}-{v}"))))?,
            (vec!["3-Three".into(), "0-Zero".into(),], None)
        );

        assert_eq!(
            Database::paginate_table_rev(&table, Some(3), 3, |k, v| Ok(Some(format!("{k}-{v}"))))?,
            (vec!["3-Three".into(), "0-Zero".into(),], None)
        );

        assert_eq!(
            Database::paginate_table_rev(&table, None, 2, |k, v| Ok((k % 2 == 0).then_some(v)))?,
            (vec!["Zero".into()], None)
        );

        Ok(())
    })
    .await?;

    Ok(())
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn paginate_partition() -> BoxedErrorResult<()> {
    let (_dir, db) = temp_db_rng().await?;

    db.write_with(|tx| {
        let mut table = tx.open_table(&test_table::TABLE)?;

        // Insert test data across different ranges
        table.insert(&10, "Ten")?;
        table.insert(&11, "Eleven")?;
        table.insert(&20, "Twenty")?;
        table.insert(&21, "Twenty-One")?;
        table.insert(&30, "Thirty")?;

        // Test partition range 10..=19
        assert_eq!(
            Database::paginate_table_partition(
                &table,
                10..=19,
                |c: u64| c,
                None,
                1,
                |k, v| Ok(Some(format!("{k}-{v}")))
            )?,
            (vec!["10-Ten".into()], Some(11))
        );

        // Test with cursor in first partition
        assert_eq!(
            Database::paginate_table_partition(
                &table,
                10..=19,
                |c: u64| c,
                Some(11),
                1,
                |k, v| Ok(Some(format!("{k}-{v}")))
            )?,
            (vec!["11-Eleven".into()], None)
        );

        // Test partition range 20..=29
        assert_eq!(
            Database::paginate_table_partition(
                &table,
                20..=29,
                |c: u64| c,
                None,
                2,
                |k, v| Ok(Some(format!("{k}-{v}")))
            )?,
            (vec!["20-Twenty".into(), "21-Twenty-One".into()], None)
        );

        // Test with filter
        assert_eq!(
            Database::paginate_table_partition(
                &table,
                10..=29,
                |c: u64| c,
                None,
                3,
                |k, v| Ok((k % 2 == 0).then_some(format!("{k}-{v}")))
            )?,
            (vec!["10-Ten".into(), "20-Twenty".into()], None)
        );

        Ok(())
    })
    .await?;

    Ok(())
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn paginate_partition_rev() -> BoxedErrorResult<()> {
    let (_dir, db) = temp_db_rng().await?;

    db.write_with(|tx| {
        let mut table = tx.open_table(&test_table::TABLE)?;

        table.insert(&9, "Nine")?;
        table.insert(&10, "Ten")?;
        table.insert(&11, "Eleven")?;
        table.insert(&19, "Nineteen")?;
        table.insert(&20, "Twenty")?;
        table.insert(&21, "Twenty-One")?;
        table.insert(&30, "Thirty")?;

        let (first_page, cursor) = Database::paginate_table_partition_rev(
            &table,
            10..=19,
            |c: u64| c,
            None,
            1,
            |k, v| Ok(Some(format!("{k}-{v}"))),
        )?;
        assert_eq!(first_page, vec!["19-Nineteen"]);
        assert_eq!(cursor, Some(11));

        let (second_page, cursor) = Database::paginate_table_partition_rev(
            &table,
            10..=19,
            |c: u64| c,
            cursor,
            1,
            |k, v| Ok(Some(format!("{k}-{v}"))),
        )?;
        assert_eq!(second_page, vec!["11-Eleven"]);
        assert_eq!(cursor, Some(10));

        let (third_page, cursor) = Database::paginate_table_partition_rev(
            &table,
            10..=19,
            |c: u64| c,
            cursor,
            1,
            |k, v| Ok(Some(format!("{k}-{v}"))),
        )?;
        assert_eq!(third_page, vec!["10-Ten"]);
        assert_eq!(cursor, None);

        assert_eq!(
            Database::paginate_table_partition_rev(
                &table,
                10..=19,
                |c: u64| c,
                Some(11),
                2,
                |k, v| Ok(Some(format!("{k}-{v}")))
            )?,
            (vec!["11-Eleven".into(), "10-Ten".into()], None)
        );

        assert_eq!(
            Database::paginate_table_partition_rev(
                &table,
                10..=19,
                |c: u64| c,
                Some(30),
                1,
                |k, v| Ok(Some(format!("{k}-{v}")))
            )?,
            (vec!["19-Nineteen".into()], Some(11))
        );

        assert_eq!(
            Database::paginate_table_partition_rev(
                &table,
                19..=19,
                |c: u64| c,
                None,
                1,
                |k, v| Ok(Some(format!("{k}-{v}")))
            )?,
            (vec!["19-Nineteen".into()], None)
        );

        assert_eq!(
            Database::paginate_table_partition_rev(
                &table,
                20..=29,
                |c: u64| c,
                None,
                2,
                |k, v| Ok(Some(format!("{k}-{v}")))
            )?,
            (vec!["21-Twenty-One".into(), "20-Twenty".into()], None)
        );

        let (filtered_page, cursor) = Database::paginate_table_partition_rev(
            &table,
            10..=20,
            |c: u64| c,
            None,
            1,
            |k, v| Ok((k % 2 == 0).then_some(format!("{k}-{v}"))),
        )?;
        assert_eq!(filtered_page, vec!["20-Twenty"]);
        assert_eq!(cursor, Some(19));

        assert_eq!(
            Database::paginate_table_partition_rev(
                &table,
                10..=20,
                |c: u64| c,
                cursor,
                1,
                |k, v| Ok((k % 2 == 0).then_some(format!("{k}-{v}")))
            )?,
            (vec!["10-Ten".into()], None)
        );

        let (empty_page, cursor) = Database::paginate_table_partition_rev(
            &table,
            10..=19,
            |c: u64| c,
            None,
            0,
            |k, v| Ok(Some(format!("{k}-{v}"))),
        )?;
        assert!(empty_page.is_empty());
        assert_eq!(cursor, Some(19));

        assert_eq!(
            Database::paginate_table_partition_rev(
                &table,
                10..=19,
                |c: u64| c,
                cursor,
                1,
                |k, v| Ok(Some(format!("{k}-{v}")))
            )?,
            (vec!["19-Nineteen".into()], Some(11))
        );

        Ok(())
    })
    .await?;

    Ok(())
}
