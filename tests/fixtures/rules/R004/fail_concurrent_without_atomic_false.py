# R004: Migration uses concurrent operations but is not marked as non-atomic - should fail
from django.db import migrations, models
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0001_initial'),
    ]

    operations = [
        AddIndexConcurrently(
            model_name='order',
            index=models.Index(fields=['created_at'], name='order_created_idx'),
        ),
    ]
